# ADR-007: Capability-aware human interaction routing

## Status

Accepted. Retrospective record written on 2026-10-24 for implemented behavior.
Refines the interaction responsibilities of the controller in
[ADR-001](001-session-control-and-replay.md).

## Context

Agent questions can outlive a pilot connection, overlap other questions, or
belong to one specific live RPC. Broadcasting approval requests to observers
would violate ownership. Pinning all questions to the original pilot would
break handoff. Waiting inside dispatch would block updates and cancellation.

Clients also have different input capabilities. A missing form or URL UI cannot
be interpreted as permission to proceed.

## Decision

Route session-scoped interactions to the current pilot. Route request-scoped
elicitation through a uniquely mapped live parent RPC to its original requester.
Translate connection-local request and cancellation IDs between hops.

Keep human waits outside dispatch, bounded, and cancellable. Preserve concurrent
request identities and per-question drafts. Unsupported modes, unmapped parents,
timeouts, and invalid responses cancel safely rather than auto-approve.

Validate replies and reject stale deliveries. Treat URL consent separately from
external completion. Use shared validation fixtures to keep native and browser
implementations aligned without assuming they execute the same code.

## Consequences

- Handoff remains possible without authorizing an old pilot's response.
- Correlation, cancellation, queue, and completion state add complexity.
- Unsupported schemas cannot simply be reduced to arbitrary text fields.
- Cross-host validation deliberately supports a common subset.

Implementation: [daemon routing](https://github.com/noirbizarre/hive/blob/main/crates/hive-daemon/src/interactions.rs),
[native validation](https://github.com/noirbizarre/hive/blob/main/crates/hive-protocol/src/interactions.rs), and
[scripted interactions](https://github.com/noirbizarre/hive/blob/main/crates/hive-daemon/tests/support/interactions.rs).
See the [interaction contract](../acp-compatibility.md#interaction-capabilities-and-fallback).

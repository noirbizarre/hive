# ADR-004: Native ACP bridge for Zellij

## Status

Accepted. Retrospective record written on 2026-10-24 for implemented behavior.
Replaces the status-only Zellij limitation described in
[ADR-001](001-session-control-and-replay.md), while retaining its session contract.

## Context

Zellij's WebAccess host API completes individual HTTP requests. It cannot hold
the ACP WebSocket or SSE streams needed for a full conversation client.

Keeping ZJHive status-only would prevent handoff. Implementing persistent ACP
inside the plugin would exceed its host interface. Adding separate daemon
mutation routes for Zellij would create another session-control protocol.

## Decision

Use native `hive zellij-bridge` as an ordinary ACP client. Each plugin
registration gets its own upstream connection. The plugin uses short local HTTP
requests to exchange actions and bounded snapshots, and polls daemon discovery.

The bridge owns transient client state, not sessions. Detach, helper exit, or
handle expiry drops connectivity without closing the daemon-owned conversation.
Interaction deliveries carry identities so stale responses cannot approve a
later question.

## Consequences

- The native executable must be installed in Zellij's environment.
- WebAccess and RunCommands permissions are required.
- Another loopback listener, polling latency, and helper lifecycle need handling.
- Bounded snapshots are not independent durable history.
- Ownership, fencing, and downstream lifetime stay in the shared broker.

Implementation: [bridge](https://github.com/noirbizarre/hive/blob/main/crates/hive-cli/src/zellij_bridge.rs)
and [plugin](https://github.com/noirbizarre/hive/blob/main/crates/hive-zellij/src/lib.rs).
See the [user guide](../guides/zellij.md).

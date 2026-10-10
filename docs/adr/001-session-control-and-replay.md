# ADR-001: In-memory fenced controller and sequenced replay

## Status

Accepted

## Context

ACPv2 sessions and a downstream agent can outlive an upstream transport, but
ACPv2 has no standard exclusive control lease or reliable event cursor. A
client can lose its prompt acknowledgement while its agent keeps working.
Binding control to a WebSocket would exclude the SDK's HTTP/SSE alternative,
and retrying an uncertain prompt without an idempotency key could duplicate it.

## Decision

Keep the session, downstream connection and agent process in the daemon's
shared broker. Store one controller generation and opaque token per Hive
session. New and resume return these in ACP `_meta.hive`; resume rotates them
atomically and fences prior controllers. No timed lease is used: a disconnected
client leaves its session alive; any other client can explicitly take over.
Mutating commands must carry the latest ownership metadata; prompt keys dedupe
same-content retries. Busy turns reject additional prompts until `idle`, while
handoff does not affect active work. Keep a 256-update in-memory sequence log
and replay through an inclusive `_hive_seq` cursor; reject gaps before takeover.
The daemon serializes replay, takeover and live forwarding per session.

## Consequences

An untrusted local process can take over under the existing single-user
loopback policy; this is fencing against accidental stale clients, not
multi-user authentication. ACP cancel is a notification and cannot report a
stale-owner error to the sender. Running work and prompt outcomes survive
upstream disconnects but neither sessions nor replay survive daemon restart.
The bounded log requires clients to handle an expired cursor explicitly.
ZJHive remains status-only until its host interface can drive a suitable
client transport; no separate session manager is created for it.

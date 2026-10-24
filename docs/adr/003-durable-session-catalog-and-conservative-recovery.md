# ADR-003: Durable catalog and conservative recovery

## Status

Accepted. Retrospective record written on 2026-10-24 for implemented behavior.
Extends [ADR-001](001-session-control-and-replay.md) without replacing its
controller or volatile replay design.

## Context

Surviving a client disconnect is different from surviving daemon restart.
Persisting metadata cannot preserve a subprocess or prove whether interrupted
work already changed files, called tools, or incurred model costs.

An in-memory-only catalog loses identities. Full event sourcing would retain
more history but requires a durable event contract and does not itself restore
downstream work. Automatically recreating sessions or resending prompts risks
duplicate conversations and external effects.

## Decision

Use SQLite WAL for durable session identity and recovery bookkeeping. Persist
downstream IDs, profile/adapter choices, prompt content, reservations/results,
usage, and recovery metadata. Keep replay payloads, activity snapshots, and
controller credentials volatile.

Reattach the original downstream ID. Never create a replacement or resend an
uncertain prompt automatically. Distinguish unavailable, orphaned, and lost
records. Permit observation of reattached uncertain sessions, but require explicit
acknowledgement before admitting new work.

Reserve sequence blocks durably before publishing updates so a crash cannot
reuse an already published cursor. Explicit close removes the record; daemon
shutdown preserves it.

## Consequences

- Recovery remains dependent on downstream persistence and capabilities.
- History gaps are visible. Durable bookkeeping is not a transcript archive.
- Acknowledgement permits new work but cannot certify old external effects.
- Prompt contents/results require private storage and careful backup handling.
- SQLite schema migration and storage failure become operational concerns.

Implementation: [persistence.rs](https://github.com/noirbizarre/hive/blob/main/crates/hive-daemon/src/persistence.rs),
[broker](https://github.com/noirbizarre/hive/blob/main/crates/hive-daemon/src/lib.rs), and
[gateway tests](https://github.com/noirbizarre/hive/blob/main/crates/hive-daemon/tests/gateway.rs).

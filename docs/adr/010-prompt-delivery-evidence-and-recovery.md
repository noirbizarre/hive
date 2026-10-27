# ADR-010: Prompt delivery evidence and recovery acknowledgement

## Status

Accepted. Extends ADR-003 and ADR-009. Implements issue #56 within Hive.

## Context

A durable reservation prevents duplicate dispatch, but a generic failure marker
does not distinguish rejected work from a lost response after acceptance. Current
downstream protocols do not expose a durable lookup by Hive's request key. An
idle session or plausible transcript cannot establish which request executed or
recover its original acknowledgement. Acknowledging uncertain external effects
is different from establishing acceptance, completion, or transcript coverage.

## Decision

- Preserve the original session, key and fingerprint. Every reserved key remains
  a tombstone; receipt retrieval never redispatches it.
- Commit a dispatch-possible marker before publication. New reservations without
  that marker prove non-dispatch after restart; legacy null rows do not. A crash
  after the marker may remain uncertain even if publication had not yet occurred.
- Keep minimal delivery states, acknowledgement IDs, controlled terminal evidence
  and recovery acknowledgement separately. Receipt and reliably correlated live
  terminal evidence settle normal work regardless of their arrival order.
- Treat ambiguous post-publication failures as uncertainty and fence new work.
  Neither later idle nor historical messages manufacture a lost receipt/result.
- Acknowledge current recovery incidents durably without erasing their unknown
  outcomes or releasing keys. New uncertainty raises a fresh fence. Active work
  remains busy even after acknowledgement.
- Expose controlled stable diagnostic codes in JSON-RPC error data. Clients observe
  after reconnect, explicitly retrieve receipts, and acknowledge recovery without
  implicitly skipping history or reporting idle.
- Migrate to schema 3 without bodies or arbitrary downstream payloads. Legacy
  ambiguous outcomes may require a one-time acknowledgement because earlier flags
  cannot distinguish explicit acceptance of risk from automatic clearing.

## Consequences

At-most-once dispatch survives loss of either process and acknowledgement. Some
prompts remain explicitly uncertain until the user inspects external effects;
even acknowledgement cannot establish exactly-once completion. Reconciliation
can use recorded receipts and correlated live terminal evidence, but stronger
lost-response recovery needs a future negotiated downstream receipt contract.

Hive remains a minimal safety journal and session registry, not a transcript
store. Transcript availability and completeness remain independent. Additional
write-ahead transactions cost storage writes in exchange for a verifiable boundary
between proven non-dispatch and possible effects.

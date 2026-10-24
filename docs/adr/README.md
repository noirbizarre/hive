# Architecture Decisions

Records of the decisions that shape this project, and — more usefully — the
reasons behind them. An ADR is written when a choice is hard to reverse or
likely to be re-proposed.

The point is not the decision; it is the alternatives that were rejected and
why. A record that only states the outcome saves nobody the argument.

A decision is changed by writing a new ADR that supersedes the old one, never
by editing the old one. The history is the value.

## Format

`NNN-kebab-case-title.md`, numbered in the order written, with the sections:

- **Status** — Proposed, Accepted, or Superseded by ADR-NNN
- **Context** — the forces in play, before any decision
- **Decision** — what was decided
- **Consequences** — what this costs, including what it makes harder

## Index

- [ADR-001](001-session-control-and-replay.md) — In-memory fenced controller and sequenced replay
- [ADR-002](002-opencode-server-adoption.md) — Adopt OpenCode sessions through the server API, not ACP
- [ADR-003](003-durable-session-catalog-and-conservative-recovery.md) — Durable catalog and conservative recovery
- [ADR-004](004-zellij-native-acp-bridge.md) — Zellij transport history and independent ZJHive ownership
- [ADR-005](005-explicit-downstream-protocol-compatibility.md) — Explicit downstream protocol compatibility
- [ADR-006](006-local-operational-http-and-acp-boundary.md) — Local operational HTTP and ACP boundary
- [ADR-007](007-capability-aware-human-interaction-routing.md) — Capability-aware human interaction routing

## Subsequent decisions and current interpretation

ADR-001 and ADR-002 preserve their historical context. Their central decisions
remain accepted. Later records describe implemented extensions:

- ADR-003 adds durable catalog recovery without making replay durable.
- ADR-004 records the former status-only limitation and subsequent independent ZJHive ownership.
- ADR-007 specifies session-scoped versus request-scoped human input routing.

For current setup and API details, use the [configuration reference](../configuration.md)
and [OpenCode guide](../agents/opencode.md). In particular, adoption requires an
explicit profile selector, and duplicate protection is profile-scoped.

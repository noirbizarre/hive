# ADR-004: Native ACP bridge for Zellij

## Status

Superseded in ownership by the independent
[ZJHive repository](https://github.com/noirbizarre/zjhive), implementing
[Hive #37](https://github.com/noirbizarre/hive/issues/37).
This page retains the historical architectural boundary, not installation instructions.

## Context

The original integration used a Hive-owned native adapter because Zellij's
completion-based WebAccess API could not supply persistent ACP conversations.
This replaced [ADR-001](001-session-control-and-replay.md)'s status-only limitation
without adding another daemon mutation API.

## Decision

Hive must not contain, build, launch or maintain a Zellij-specific native bridge.
ZJHive owns its complete integration, including any required transport companion.
The companion is an ordinary client of Hive's public ACP and operational interfaces.
Session ownership, control fencing and downstream workers remain in Hive.

ZJHive's transport investigation and real Zellij 0.45.1 smoke tests establish its
selected adapter's host compatibility. That evidence and repeatable probes belong
to ZJHive alongside its build/test/release configuration.

## Consequences

- Hive installation and CI require no Zellij SDK, WASM target or helper.
- Plugin and companion distribution/version compatibility belong to ZJHive.
- A transport adapter may retain bounded client snapshots and retry identity,
  but detach, helper exit and registration expiry must never close Hive sessions.
- The shared ACP contract remains authoritative; no parallel session mutation API
  is introduced for an individual client.

# Status and roadmap

This page describes the implementation in this checkout as of 2026-10-24.
It does not assert the current state of linked issues or a published release.
The [compatibility matrices](acp-compatibility.md) distinguish deterministic
coverage from retained real-agent evidence.

## Implemented foundation

| Capability | Current boundary | Background |
| --- | --- | --- |
| Shared sessions and transferable control | One pilot, observers, stale-owner fencing, bounded replay | [#3](https://github.com/noirbizarre/hive/issues/3) |
| Agent compatibility probes | Explicit ACP versions, initialize-only doctor, opt-in model turns | [#4](https://github.com/noirbizarre/hive/issues/4) |
| TUI and browser handoff | Same Hive session and downstream conversation | [#7](https://github.com/noirbizarre/hive/issues/7), [#8](https://github.com/noirbizarre/hive/issues/8) |
| Independent ZJHive integration | Plugin and companion owned by [ZJHive](https://github.com/noirbizarre/zjhive) | [#37](https://github.com/noirbizarre/hive/issues/37) |
| Durable catalog and restart recovery | Saved-ID reattachment, explicit uncertainty, volatile replay | [#10](https://github.com/noirbizarre/hive/issues/10) |
| Live execution activity | Tools, plans, foreground state, explicit task events and snapshots | [#17](https://github.com/noirbizarre/hive/issues/17) |
| Token usage and cost estimates | Reported turn totals and exact optional pricing mappings | [#18](https://github.com/noirbizarre/hive/issues/18) |
| Pi ACP adapter setup | External `pi-acp`; retained evidence is initialization only | [#19](https://github.com/noirbizarre/hive/issues/19) |
| OpenCode native adoption | Explicit server adapter; API workflow, not a client adoption picker | [#5](https://github.com/noirbizarre/hive/issues/5) |

These are implemented capabilities, not blanket guarantees for every installed
agent. In particular, a passing initialize probe does not establish model turns,
restart recovery, permissions, or native adoption.

## Remaining integration work

- **Pi native adoption:** [#6](https://github.com/noirbizarre/hive/issues/6).
  ACP adapter compatibility alone does not establish native-session import.
- **Broader real-agent evidence:** retain versioned prompt, streaming, cancel,
  interaction, disconnect, and recovery results before promoting matrix cells.
- **OpenCode preview API:** keep it experimental and distinct from v1 stores.
  Its questions are unsupported, and coverage is primarily fake-server based.
- **External client compatibility:** ZJHive maintains its own build, distribution
  and real-host evidence against Hive's ACP interfaces.

## Current client limits

The browser creates new sessions with cwd `/`, without a project picker.
Use the TUI for project-session creation. ZJHive opens existing sessions and
does not provide interrupted-work acknowledgement. Native adoption currently
requires an ACP integration client rather than a supplied UI or CLI action.

These limits describe current behavior rather than committed release dates.

## Architectural invariants

- One daemon owns the shared catalog in a local runtime namespace.
- Client connections, Hive sessions, downstream workers, and downstream IDs
  have separate lifetimes.
- Handoff fences old controllers without restarting active work.
- Client detach neither closes a session nor cancels its agent.
- Stable prompt keys prevent an identical logical retry from redispatching work.
- Recovery does not create replacements or automatically resend uncertain work.
- Client-facing ACPv2 stays separate from explicit downstream adapters.
- The trust boundary is one local user, not remote multi-user authorization.

The [ADRs](adr/README.md) explain these choices. Deterministic gateway and
OpenCode adapter tests exercise their contracts.

## Release management

Releases are driven by gh-ship. Generated release PRs and version changes are
release artifacts, not evidence that implementation or compatibility is complete.
Review the intended scope, CI, relevant client smoke tests, and retained agent
evidence before merging a release PR.

Never bump versions, push tags, or edit generated release content by hand.
The checked-in publish workflow does not currently upload binary assets, so
the [installation guide](getting-started.md#1-install-hive) documents source installation.

See [contribution instructions](https://github.com/noirbizarre/hive/blob/main/CONTRIBUTING.md#releases)
for the release contract and template maintenance.

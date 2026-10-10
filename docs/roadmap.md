# Roadmap

Hive's goal is to provide one shared session manager and a unified interface for
ACP coding agents. A session has a stable identity independent of the client
that created or currently controls it. **Only one client controls a session at
a time**, but control can move between the TUI, WebUI, ZJHive, and eventually
the agents' native interfaces.

The first release prioritizes sessions created through Hive. Attaching to
sessions created directly in OpenCode or Pi is a planned later capability, not
a prerequisite for the first release.

## Current focus

### Phase 0 — Stabilize the bootstrap

- Finish fixing CI and keep `mise run ci` green.
- Preserve the `rust.tpl` / `git-tpl` workflow; inspect template-owned changes
  before editing generated files.

### Phase 1 — Establish session ownership and verify agents

These can proceed in parallel:

- [#3 — Shared session manager with exclusive, transferable control](https://github.com/noirbizarre/hive/issues/3)
  - Stable Hive session IDs; separate session, client, agent process/connection,
    and downstream ACP session lifetimes.
  - Atomic control handoff, stale-controller rejection, detach/reconnect, event
    cursors/replay, and no duplicate prompts or generations.
- [#4 — Real-agent ACP compatibility and lifecycle tests](https://github.com/noirbizarre/hive/issues/4)
  - Establish what installed Pi and OpenCode versions actually support.
  - Keep real-agent tests opt-in; normal CI remains deterministic.

**This is the foundational phase.** Define the session and handoff contract here
before implementing independent lifecycle logic in any client.

### Phase 2 — Deliver the unified TUI and cross-client handoff

Once the shared session contract is implemented, work on the clients:

- [#7 — Unified Hive TUI](https://github.com/noirbizarre/hive/issues/7) — primary
  interface; create, list, resume, and control sessions with consistent tool,
  permission, input, and cancellation handling.
- [#8 — WebUI session resume and control](https://github.com/noirbizarre/hive/issues/8)
  — open the same session ID, take control, send another prompt, and reconnect
  without forking the conversation.
- [#9 — ZJHive attach and prompt submission](https://github.com/noirbizarre/hive/issues/9)
  — select an existing session and take control from within Zellij. ZJHive can
  explore Zellij-specific navigation/orchestration separately, but must reuse
  the daemon's session manager.

The target workflow is: start in the TUI, continue in the WebUI, then resume or
submit another prompt from ZJHive. Every transition uses the same Hive session
and downstream agent session. The previous client loses mutation rights when
control is transferred.

### Phase 3 — Persist and recover Hive sessions

- [#10 — Persist session metadata and recover after daemon restart](https://github.com/noirbizarre/hive/issues/10)

Do this after the lifecycle and recovery capabilities are understood. Persisting
a session record does not necessarily mean an agent process or active generation
can be restored. Report recovery state honestly; never silently create a
replacement session or resend a prompt.

### Phase 4 — Adopt sessions created in native agent interfaces

These are separate, longer-term integrations and are not first-release blockers:

- [#5 — OpenCode server v2 persistent sessions](https://github.com/noirbizarre/hive/issues/5)
  — discover and attach to sessions created directly in OpenCode, and resume
  Hive-created sessions there where its server APIs allow it.
- [#6 — Pi persistent sessions](https://github.com/noirbizarre/hive/issues/6)
  — discover and attach to sessions created directly in Pi, and resume
  Hive-created sessions in Pi where its supported persistence APIs allow it.

Both depend on the shared session contract and real-agent capability testing.
ACP compatibility alone does not guarantee that an existing native session can
be adopted. Do not silently create a duplicate conversation when adoption is
unsupported.

## Architectural invariants

- One Hive daemon per local user; clients never launch their own agent for an
  existing Hive session.
- A Hive session, client connection, downstream agent process/connection, and
  downstream session have distinct identities and lifetimes.
- At most one client can issue state-changing operations for a session at once.
- A handoff invalidates the old controller; stale commands are rejected.
- Detaching a client does not stop an active generation.
- Reconnect and replay must not duplicate messages, prompts, or generations.
- Keep the client-facing protocol ACPv2; use explicit adapters for agent-specific
  or native-server integration.
- Keep the current local-trusted-user security boundary explicit; do not imply
  remote multi-user authorization exists.
- Update tests and documentation with each issue. Preserve template-owned
  configuration and use the repository's documented `mise` tasks.

## Suggested implementation order

1. Finish CI stabilization.
2. Implement #3; start #4 in parallel if convenient.
3. Implement #7 as the primary product client.
4. Implement #8 and #9 against the same handoff API.
5. Implement #10 once session recovery semantics are explicit.
6. Implement #5 and #6 as separate optional integrations after validating each
   agent's actual native session APIs.

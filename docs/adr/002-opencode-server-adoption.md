# ADR-002: Adopt OpenCode sessions through the server API, not ACP

## Status

Accepted

## Context

Issue #5 asks that a conversation started in OpenCode can be continued through
Hive, and that a Hive-created session can be resumed in OpenCode, without ever
creating a second conversation.

`opencode acp` can already `session/list` and `session/load` native sessions
(observed on OpenCode 1.19.3), but it is a *separate process per Hive session*
that Hive would spawn. It cannot observe a turn that another OpenCode client
(the TUI) is running, and it offers no way to learn that someone else is
driving the session. The TUI and `opencode serve` expose the live session
through a REST + SSE server instead.

That server has two API families on 1.19.3:

- the documented **v1** routes the TUI uses (`/session`, `/event`,
  `prompt_async`, `/permission/*`, `/question/*`). Its event stream has no
  replay cursor;
- the **`/api/*` preview** with sequence-numbered durable events, `/interrupt`
  and `/wait`. In 1.19.3 its message store is separate from v1: a prompt sent
  through v1 is absent from `/api/session/{id}/history`.

OpenCode has no arbitration between clients. Any client can prompt, abort or
answer a permission at any time.

## Decision

Add an explicit `kind = "opencode-server"` profile and a dedicated adapter
(`crates/hive-daemon/src/opencode/`) that plugs into the existing per-session
worker seam. Hive-facing behaviour stays ACPv2; control, fencing, replay and
pilot routing are the shared code, not a copy.

- **Identity.** One native session maps to exactly one Hive session.
  Adoption is `session/new` with `_meta.hive.adopt = {"nativeId": "ses_…"}`;
  it attaches to an existing session and never calls session creation. A second
  adoption fails with `hive::opencode::already_adopted` carrying the existing
  Hive ID. The native ID, API flavor and origin (`hive` or `adopted`) are
  persisted, and the native ID is exposed in the catalog so the session can be
  resumed in OpenCode (`opencode attach <url> -s <id>`).
- **Discovery** is a read-only HTTP route,
  `GET /hive/v1/providers/{agent}/native-sessions`, that lists candidates and
  marks the adopted ones. The catalog is never auto-populated with sessions
  nobody asked Hive to manage.
- **API flavor** is probed (`/global/health`, `/api/health`, validating the
  JSON body) and **pinned per session at attach time**. v1 is preferred when
  the session is visible there; v2 is used only when v1 is unavailable or does
  not know the session. Recovery honours the pinned flavor or fails; it never
  swaps, because the stores differ.
- **No silent fallback.** Unsupported versions, missing APIs, a missing
  session or a directory mismatch fail with a `hive::opencode::*` diagnostic.
  A session missing after restart becomes `lost`, never a replacement.
- **External controller.** Hive cannot lock the native client out. A turn Hive
  did not start is detected (`busy` with no Hive prompt in flight), marks the
  session `externalActivity`, publishes `running`, and refuses Hive prompts with
  `hive::opencode::externally_controlled` until it is idle. Cancelling is still
  allowed to the pilot.
- **Close detaches.** Closing a Hive session never deletes the OpenCode one.

## Consequences

The single-controller guarantee holds among Hive clients; against OpenCode's own
clients Hive can only detect and refuse, not prevent. v1 reconnection has no
cursor, so after a dropped stream the worker re-reads status and recent history
and relies on idempotent per-part tracking to avoid duplicating text; a very
short tool-state transition inside the gap may be observed only in its final
state. Adoption backfills a bounded window (50 messages) into the 256-update
replay ring, so earlier history is only in OpenCode. The v2 preview is
supported for sessions, prompts, interrupts, permissions and events, but not for
questions, and is experimental until OpenCode stabilises it.

Recovery classifies failures by diagnostic code, so renaming a code is a
breaking change to both users and the recovery logic.

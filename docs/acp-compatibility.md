# ACP compatibility and draft maturity

Hive clients always initialize with ACP protocol version 2. The current SDK
`agent-client-protocol =3.3.0` needs its `unstable_protocol_v2` feature;
`agent-client-protocol-http =3.3.0` provides both server transport shapes.
Their crate versions do not denote the ACP wire version. The SDK pins schema
`=1.11.0`; the Cargo lockfile records the tested dependency set.

Implemented v2 operations: initialize, session/new, list, resume, close,
prompt, cancel and session/update. A prompt response acknowledges acceptance;
observe `running` then `idle` for work completion. Only text prompts are
adapted to v1. V1 message chunks are forwarded with a synthetic v2 message
ID; its completed prompt response generates a v2 idle update. V1 tool calls
are preserved as `_hive_v1_tool_call`/`_hive_v1_tool_update` events; permissions
and supported elicitation are forwarded to the pilot. V2 permissions and
session-scoped form/URL elicitation are forwarded to the pilot with a 120-second
unanswered-request timeout. Request-scoped/unknown elicitation modes are
cancelled. MCP bridge, forks, config options, compaction and other optional
capabilities are **not** advertised. Non-text v1 prompts return an error.

ACPv2 `session/new` has no standard agent selector. Hive reserves
`_meta.hive.agent` for a profile ID in **client-to-Hive** requests; Hive removes
the selector before forwarding to downstream agents. Omit it to use
`default_agent`; if no default exists, creation returns an error. Example:

```json
{"cwd":"/workspace","mcpServers":[],"_meta":{"hive":{"agent":"pi"}}}
```

The same request works over WebSocket and HTTP/SSE. The SDK supports `_meta`
on `v2::NewSessionRequest`; this selector is a Hive extension, not an ACP
standard field. Profile-declared versions and transports select one downstream
connection without speculative retry: v2 first where declared, otherwise the
explicit v1 compatibility adapter. SDK client transports support stdio,
WebSocket and HTTP/SSE. No automatic fallback on failed initialization is
claimed.

## Installed-agent capability matrix (2026-10-11)

These rows identify **inspected local binaries**, not validated live model runs.
`Guaranteed` means demonstrated for the specified installation by a completed
probe; `best-effort` means observed but dependent on timing/model/configuration;
`unsupported` means a missing entrypoint or Hive operation; `unverified` means
no successful live probe. The v1/v2 declaration on a profile does **not** prove
the downstream agent accepts that wire version. Re-run the opt-in probes after
upgrading either agent and record results with its product version and transport.

| Product/version | ACP entrypoint/transport | ACP wire | Fresh `session/new` | Native-created load/resume over ACP | Hive live-worker resume |
| --- | --- | --- | --- | --- | --- |
| Pi 1.1.0 (local CLI inspection) | Default CLI offers text/json/rpc; ACP entrypoint unsupported without verified extension | Unverified | Unverified | Native `--session-id` CLI available; ACP attachment unverified | Unverified with Pi (guaranteed with mock) |
| OpenCode 1.18.35 (local CLI inspection) | `opencode acp`, stdio documented and command present | v1 documented; v2 unverified | Unverified | Native `session list` and `run --session` available; ACP load unverified | Unverified with OpenCode (guaranteed with mock) |

| Product/version | Text prompt | Streaming events/idle | Tool calls/results | Permissions | User-input requests | Cancellation | Active client disconnect |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Pi 1.1.0 | Unverified | Unverified | Unverified | Unverified | Unverified | Unverified | Unverified |
| OpenCode 1.18.35 | Unverified | Unverified | Unverified | Unverified | Unverified | Unverified | Unverified |

The OpenCode ACP command/documented v1 behavior comes from its
[ACP CLI reference](https://opencode.ai/docs/acp/); its implementation may
change between releases. Pi's CLI session flags demonstrate *native* persistence,
not ACP support. For each successful local run, retain the `PROBE`, `FIRST`,
`RESUMED` or `NATIVE` summary from `--nocapture`, the run date, and the selected
wire version before upgrading any cell to guaranteed/best-effort. Failed
initialization is **not** evidence that session creation, load, or prompts work.

For both agents, Hive's upstream ACPv2 resume is guaranteed by deterministic
mock tests **only for an existing live Hive session**; native session adoption
through Hive remains unsupported. Daemon-restart recovery attempts the saved
downstream ID through v2 `session/resume` or advertised v1 `session/resume` or
`session/load`. Capability negotiation does not guarantee downstream persistence:
failure retains a visible offline record, never a replacement session. Direct native ACP
`session/load` (v1) or `session/resume` (v2) probes bypass Hive and must be
recorded independently. Hive forwards downstream permissions and supported
elicitation modes to the current pilot; this is covered by scripted agents,
not by either installed product row. A tool-free probe does not establish
support for those optional features. See
[the reproducible probes](development.md#real-agent-compatibility-probes).

The draft [v2 specification](https://github.com/agentclientprotocol/agent-client-protocol/tree/main/docs/protocol/v2)
and [draft announcement](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/announcements/acp-v2-draft.mdx)
may change. See the [SDK v2 guide](https://github.com/agentclientprotocol/rust-sdk/blob/main/md/protocol-v2.md)
and [HTTP guide](https://github.com/agentclientprotocol/rust-sdk/blob/main/md/http-transport.md).
ACPv2 does not specify transferable control, command idempotency, or a
standard event sequence cursor. Hive extends `_meta.hive` on its upstream
requests/responses/notifications; it strips that metadata before forwarding
downstream. A new session returns `generation`, opaque `token`, `nextSeq`,
`running`, `pilot: true` and `role: "pilot"` in response `_meta.hive`. Default
resume takes pilot control. `_meta.hive.mode: "observe"` attaches read-only;
`_meta.hive.mode: "relinquish"` requires the pilot's generation/token and makes
it an observer, leaving no pilot until explicit takeover. Both modes optionally
replay retained events before returning. Observer responses have no token.
No implicit new prompt is sent. TUI `--session hive-…` and WebUI's session-ID
form use this same contract. The WebUI initially requests `mode: "observe"`
and only attempts pilot resume when the user types a prompt; a pending
permission or elicitation request then follows the new pilot. Its session
list supplies the original cwd, including for sessions created in the TUI.
For example, after `session/new` returns a session ID and
`"_meta":{"hive":{"generation":1,"token":"opaque","nextSeq":1,"running":false}}`,
the controller can send:

```json
{"sessionId":"hive-…","prompt":[{"type":"text","text":"hello"}],"_meta":{"hive":{"generation":1,"token":"opaque","key":"prompt-123"}}}
```

On another connection, take over that ID (with the original `cwd`):

```json
{"sessionId":"hive-…","cwd":"/workspace","replayFrom":{"type":"_hive_seq","seq":8}}
```

The pilot resume response returns a new generation/token; use those for subsequent
commands. `session/cancel` and `session/close` carry the same ownership fields
without a prompt key. Tokens are local control credentials: do not forward
them to downstream agents or print them in client logs.

To observe without taking control, send the same resume request with
`"_meta":{"hive":{"mode":"observe"}}`. The pilot can relinquish without
disconnecting by sending `"_meta":{"hive":{"mode":"relinquish", "generation":N,
"token":"opaque"}}` in `session/resume`. Both receive live updates; another
client sends default `session/resume` to become pilot. Agent permission and
elicitation requests are addressed to the current pilot only, and are
re-presented after takeover if still pending.

Every upstream `session/prompt` and `session/close` request and `session/cancel`
notification needs `_meta.hive.generation` and `_meta.hive.token` from the
latest successful new/resume response. Missing or stale owners receive an
actionable error for requests; cancel is a JSON-RPC notification, so a stale
cancel is discarded and cannot carry an error response. A subsequent resume
rotates ownership even when the old connection is still alive. A disconnected
pilot relinquishes control; another client must explicitly take over. All
attached clients receive live updates, including a sequenced `_hive_control`
event with `generation` and pilot presence but no token. `session/list` includes
profile, running, generation, pilot presence and observer count in each entry's
`_meta.hive`, along with the shared `title`, `lifecycle`, `activity`,
`lastActivityAt` and `controlOwner` summary fields. The owner is an ephemeral
connection ID, **never** the token. An absent title/timestamp/owner is `null`;
`activity: "unknown"` means no trustworthy foreground status is available.
The HTTP catalog and its SSE invalidation feed are documented in
[Hive's operational API](hive-api.md#session-catalog); ACP `session/list` and
HTTP snapshots derive their fields from the same broker. A close is explicit and cannot be issued while work is active;
client detach does not close the session or cancel the agent. A second distinct prompt
is rejected until the agent sends `idle` (not merely until its prompt response).

Prompts additionally require `_meta.hive.key`, a nonempty client-generated
idempotency key scoped to the Hive session. Retrying the *same key and content*
after an uncertain acknowledgement returns or waits for the original result,
including after takeover. The same key with changed content fails. A rejected
prompt that was never dispatched retains its error under that key; use a new
key for a newly intended prompt. Do not blindly send a new key after losing an
acknowledgement; reconnect and retry the existing key while this daemon is
alive. Reservations and confirmed responses survive restart. An interrupted
key remains reserved and is never redispatched. After inspecting the agent's
state, explicitly take control with `_meta.hive.recoveryAction: "acknowledge"`
on `session/resume`; this permits *new* keys, but does not establish that
previous work completed or allow the interrupted key to be reused. Observe
mode remains available without acknowledgement.

Every `session/update` carries `_meta.hive.seq`, an increasing per-session
integer, unchanged on replay. On `session/resume`, omit `replayFrom` to attach
without history; `{ "type": "start" }` requests the full history if retained.
`{ "type": "_hive_seq", "seq": N }` requests updates **at or after** N,
inclusive. Send `lastProcessedSeq + 1` to avoid duplicates. `nextSeq` in the
resume response identifies the first event not yet produced at the atomic
takeover boundary. The most recent 256 updates (including chunks and state
transitions) are available. An expired or future cursor fails with the current
available range and leaves the old controller in place. Live updates are
ordered after replay; a partially delivered replay must be retried using the
last actually processed sequence. The WebUI rebuilds from `start` on reload
and retains the cursor alongside rendered history only while the page is
alive; an expired cursor requires a visible attach-without-history choice.
Replay does not survive a daemon restart; prompt deduplication records do.
Cursor blocks are reserved on disk before publishing updates: a restart can
skip numbers, and an expired cursor fails rather than silently replaying an
incomplete history. Updates
can arrive before prompt responses; `idle` ends foreground work, not a
unique turn-ID boundary.

## Token usage and costs

Hive reads the optional ACP end-turn token usage from v2 `state_update` / `idle`
or from the v1 `session/prompt` result, and reads ACP `usage_update.cost` as
**authoritative cumulative session cost**. These ACP token fields are turn
aggregates, not individual model-call records: individual retries, sub-agent
calls, billable provider IDs and provider-specific token categories cannot be
inferred. The context-window `used` field is a gauge, not billable tokens.

Each accepted turn ends with one sequenced `session/update` extension
`sessionUpdate: "_hive_usage"` containing `version: 1`, `scope: "turn"`,
`source: "acp"`, `turn: { id, agent, timestamp, tokens, estimate }` and
`session: { turns, reportedTurns, tokens, estimatedCost, actualSessionCost,
contextUsed, contextSize }`. For an ACP `usage_update` or cost change, `turn`
is null; the session snapshot replaces the previous one. `turn.id` is Hive's
idempotency key, not a provider request ID. A later price lookup can publish
a replacement event for the same turn ID; clients replace rather than sum it.
Unreported/partially reported fields are null, including aggregate categories
when any contributing turn lacks that field. `totalTokens` is the agent's
reported total, not a sum inferred from other categories. Cost numbers are
USD unless the agent supplies a different currency in `actualSessionCost`;
actual cumulative cost takes precedence over estimates. An estimate includes
`input`, `output`, `cacheRead`, `cacheWrite`, `total`, `currency`, `provider`,
`model`, `retrievedAt` (Unix milliseconds) and `source` (models.dev URL).
Partial estimates have a null total. No price identity or pricing lookup means
null estimated cost, while tokens remain available.

`_meta.hive.usageSnapshot` contains the latest session aggregate atomically on
new/resume; `_meta.hive.usageSeq` marks its event cursor. Summaries from
`session/list` and the HTTP catalog also include `usage`; the HTTP catalog
includes `usageByAgent`, grouped by configured Hive profile across sessions.
An aggregate cost is null if any contributing price is missing or currencies
disagree. Accounting survives
daemon restarts, but the bounded event replay remains in-memory. A prompt
interrupted before idle has no recorded turn; do not infer usage from it.

## Live execution activity

Native ACPv2 tool, plan, and state updates remain on the shared session stream.
Immediately after each supported native update Hive publishes a sequenced
`session/update` with `sessionUpdate: "_hive_activity"`, `version: 1`, and fields
selected by `kind`:

| Kind | Fields | Meaning |
| --- | --- | --- |
| `tool` | `toolCallId`, `tool`, `source` | Current full tool-call state after applying an ACP patch or content chunk. `status` uses ACP's `pending`, `in_progress`, `completed`, `failed`, `cancelled` values. Raw input/output and result/error content are retained when sent. |
| `plan` | `planId`, `plan`, `source` | Full replacement of structured entries, including each status. A v1 plan uses the session-scoped ID `v1-plan`. |
| `foreground` | `state`, `source` | ACP foreground running, idle (with stop reason), or requires-action state. Idle ends foreground work only. |
| `session` | `status`, `source` | Hive worker ended (`ended`), distinct from turn idle or background task completion. |
| `task` | `taskId`, `task`, `source` | Explicitly identified background task update from a `_hive_task` downstream extension; unrecognized extensions are forwarded unchanged. |

`source` is `acp-v1`, `acp-v2`, `acp` (v1/v2 tool patches), or `extension`.
`_meta.hive.seq` is the ordinary per-session event cursor, including normalized
updates. Tool patch fields absent from an update retain their previous value;
explicit null clears a field; arrays replace previous arrays. Content chunks
append until a full content update replaces them. Top-level provider `_meta` is
excluded from tool/plan/task projections. IDs are scoped to the Hive session;
turn IDs, parent task IDs, and other relationships are present only when the
downstream event provides them. A tool or task update after idle never reopens
the foreground turn or closes the session.

`_meta.hive.activitySnapshot` in new/resume responses contains `{version:1,
seq,session,foreground,tools,plans,tasks,support}` at the atomic replay/live attachment
boundary. `seq` is the last normalized activity event; `nextSeq` remains the
first not-yet-produced *session update*. The support values for tools, plans,
and tasks are `observed`, `unknown`, or `unsupported`. V1 task identities are
unsupported by Hive's stable v1 adapter; otherwise an absent todo update cannot
justify an invented plan or an unsupported claim.
Unrecognized provider statuses are preserved. Finished tools are retained up
to 128 per session; streaming chunk snapshots retain the latest 64 chunks and
flag `chunksTruncated` when earlier chunks have been evicted. Raw event history
remains bounded at 256 updates (now including normalized activity), so a full
replay can expire sooner than before.

Apply replay in sequence order; ignore repeated sequence numbers. Identical
downstream content chunks without provider event IDs cannot be distinguished
from legitimate repeated chunks; entity upserts still replace the same call or
plan. On a gap,
replay from the last applied sequence plus one. If that cursor has expired,
attach live-only after an explicit history-gap decision, restore
`activitySnapshot`, and continue from `nextSeq`. A snapshot restores *current
activity*, not missed conversation text. Buffer updates that arrive before the
resume response and reduce them after the snapshot so later activity cannot be
overwritten. A daemon restart loses both history and this in-memory snapshot;
activity resumes as unknown rather than assuming the interrupted turn was idle.
V1 tool calls and plans are adapted; ACPv2 provides tool, plan, and foreground
updates natively. Background tasks require explicit provider extension events;
neither wire version guarantees them.

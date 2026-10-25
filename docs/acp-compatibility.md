# ACP compatibility and draft maturity

This is the protocol and verification reference. For setup, start with
[getting started](getting-started.md), [configuration](configuration.md), or
the [OpenCode](agents/opencode.md) and [Pi](agents/pi.md) guides.

## Protocol boundary

Hive clients always initialize with ACP protocol version 2. The current SDK
`agent-client-protocol =3.4.0` needs its `unstable_protocol_v2` feature.
`agent-client-protocol-http =3.4.0` provides both server transport shapes.
Their crate versions do not denote the ACP wire version. The SDK pins schema
`=1.12.0`. The Cargo lockfile records the resolved dependency set.

Implemented v2 operations are initialize, session/new, list, resume, close,
prompt, cancel, session/set_config_option, and session/update. A prompt response acknowledges acceptance.
Observe `running` then `idle` for work completion.

Only text prompts are adapted to v1. V1 message chunks are forwarded with a
synthetic v2 message ID. Its completed prompt response generates a v2 idle
update. V1 tool calls are preserved as `_hive_v1_tool_call`/`_hive_v1_tool_update`
events. Permissions and supported elicitation are forwarded to the pilot.

V2 permissions and
session-scoped form/URL elicitation are forwarded to the pilot with a 120-second
total decision deadline. Request-scoped elicitation follows a uniquely mapped,
live parent RPC to its original requester. Unknown modes and unmapped parent
contexts are cancelled.

MCP bridge, forks, compaction and other optional
capabilities are **not** advertised. Non-text v1 prompts return an error.

Agents can expose session configuration through `configOptions` on new/resume
responses and `config_option_update` notifications. Hive retains the current
snapshot independently of replay and forwards `session/set_config_option` for
idle sessions owned by the caller. ACPv1 option/group identities and setter
values are translated to ACPv2. Response `_meta.hive.configSeq` marks the
snapshot boundary so older replay cannot undo current selections. Support is
covered by v1/v2 mocks; actual choices depend on the downstream agent.
The native OpenCode server adapter does not expose model configuration.

Post-admission ACPv1 prompt failures produce a sequenced `_hive_error` update
before idle. Its version, source, operation, stable diagnostic code, message
and help contain controlled diagnostics rather than arbitrary provider data.
Both clients render these failures, including on replay.

ACPv2 `session/new` has no standard agent selector. Hive reserves
`_meta.hive.agent` for a profile ID in **client-to-Hive** requests. Hive removes
the selector before forwarding to downstream agents. Omit the Hive metadata
object to use `default_agent`. If `_meta.hive` is present, it must contain
`agent`, even with a configured default. Without a selector or default, creation
returns an error. Example:

```json
{"cwd":"/workspace","mcpServers":[],"_meta":{"hive":{"agent":"pi"}}}
```

The same request works over WebSocket and HTTP/SSE. The SDK supports `_meta`
on `v2::NewSessionRequest`. This selector is a Hive extension, not an ACP
standard field.

Profile-declared versions and transports select one downstream
connection without speculative retry: v2 first where declared, otherwise the
explicit v1 compatibility adapter. SDK client transports support stdio,
WebSocket and HTTP/SSE. No automatic fallback on failed initialization is
claimed.

## Installed-agent capability matrix (2026-10-11)

These rows identify **inspected local binaries**, not validated live model runs.
The evidence labels mean:

- **Guaranteed:** demonstrated for this installation by a completed probe.
- **Best-effort:** observed but dependent on timing, model, or configuration.
- **Unsupported:** missing entrypoint or Hive operation.
- **Unverified:** no successful live probe.

The v1/v2 declaration on a profile does **not** prove
the downstream agent accepts that wire version. Re-run the opt-in probes after
upgrading either agent and record results with its product version and transport.

| Product/version | ACP entrypoint/transport | ACP wire | Fresh `session/new` | Native-created load/resume over ACP | Hive live-worker resume |
| --- | --- | --- | --- | --- | --- |
| Pi 1.3.0 via external `pi-acp` 0.0.34 (see [Pi](#pi)) | `pi-acp` or `npx -y pi-acp`, stdio. Pi itself has no ACP entrypoint | v1 negotiated (2026-10-23); a v2 `initialize` is answered with v1 | Unverified (only `initialize` is verified) | Adapter advertises `loadSession` via its own session map; ACP attachment unverified; Hive adoption is [#6](https://github.com/noirbizarre/hive/issues/6) | Unverified with Pi (guaranteed with mock) |
| OpenCode 1.18.35 (local CLI inspection) | `opencode acp`, stdio documented and command present | v1 documented; v2 unverified | Unverified | Native `session list` and `run --session` available; ACP load unverified | Unverified with OpenCode (guaranteed with mock) |

| Product/version | Text prompt | Streaming events/idle | Tool calls/results | Permissions | User-input requests | Cancellation | Active client disconnect |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Pi 1.3.0 via `pi-acp` 0.0.34 | Unverified | Unverified | Unverified | Unverified | Unverified | Unverified | Unverified |
| OpenCode 1.18.35 | Unverified | Unverified | Unverified | Unverified | Unverified | Unverified | Unverified |

An ad-hoc `opencode acp` run on 1.19.3 advertised `loadSession` and loaded a
session created through `opencode serve`. That observation concerns a different
version from the 1.18.35 matrix row and has no retained `NATIVE` probe, so it
does not upgrade the row. The OpenCode ACP command/documented v1 behavior comes from its
[ACP CLI reference](https://opencode.ai/docs/acp/); its implementation may
change between releases. Pi's CLI session flags demonstrate *native* persistence,
not ACP support.

For each successful local run, retain the `PROBE`, `FIRST`,
`RESUMED` or `NATIVE` summary from `--nocapture`, the run date, and the selected
wire version before upgrading any cell to guaranteed/best-effort. Failed
initialization is **not** evidence that session creation, load, or prompts work.

For both agents over **ACP**, Hive's upstream ACPv2 resume and conversation
restoration after daemon restart are covered by deterministic v1/v2 mock tests. Native
session adoption through an ACP profile remains unsupported. OpenCode sessions
can be adopted through its server API instead. See
[the OpenCode server adapter](#opencode-server-adapter). Daemon-restart recovery attempts the saved
downstream ID through v2 `session/resume` with `replayFrom: start`, or advertised
v1 `session/load`. V1 load takes precedence over resume because v1 resume does
not replay history. A resume-only v1 agent can reattach context, but a fresh
history request returns `hive::transcript::unsupported`; explicitly attach
without earlier history to continue. Capability negotiation does not guarantee downstream persistence:
failure retains a visible offline record, never a replacement session.

Direct native ACP
`session/load` (v1) or `session/resume` (v2) probes bypass Hive and must be
recorded independently.

An initialize-only `hive doctor opencode --json` run on 2026-10-25 negotiated
ACPv1 with OpenCode 1.19.4 and reported `loadSession=true`, `resume=true`, and
`list=true`. The ignored direct native-session ACP probe also passed with a
nonempty retained conversation on that version/date; it sends no prompt and
asserts replayed message updates. This establishes direct load/replay, while
Hive restart and browser reconstruction remain covered by deterministic tests.
Runtime logs record the negotiated protocol/capabilities and chosen restoration
method. Sanitized failures distinguish `hive::transcript::session_missing` from
`hive::transcript::replay_failed`; they never create a replacement session.
If durable accounting establishes earlier completed turns but the agent returns
no conversation messages, attachment succeeds with a `hive::transcript::history_missing`
warning. Accounting is not proof of retention or a protocol violation. A genuinely
new empty session remains valid. Restoration selection follows negotiated protocol
capabilities, never agent names. V2's advertised session surface includes `start`
replay; no separate history capability is required. A successful v2 resume without
its required current-state update produces `hive::transcript::state_missing`.

Hive forwards downstream permissions and supported
elicitation modes to the current pilot. This is covered by scripted agents,
not by either installed product row. A tool-free probe does not establish
support for those optional features. See
[the reproducible probes](development.md#real-agent-compatibility-probes).

## Control and replay

The draft [v2 specification](https://github.com/agentclientprotocol/agent-client-protocol/tree/main/docs/protocol/v2)
and [draft announcement](https://github.com/agentclientprotocol/agent-client-protocol/blob/main/docs/announcements/acp-v2-draft.mdx)
may change. See the [SDK v2 guide](https://github.com/agentclientprotocol/rust-sdk/blob/main/md/protocol-v2.md)
and [HTTP guide](https://github.com/agentclientprotocol/rust-sdk/blob/main/md/http-transport.md).
ACPv2 does not specify transferable control, command idempotency, or a
standard event sequence cursor. Hive extends `_meta.hive` on its upstream
requests/responses/notifications. Control credentials are removed before
forwarding mutations downstream. On creation, recognized selector/adoption
fields are consumed; do not assume arbitrary additional Hive keys are stripped.

A new session returns `generation`, opaque `token`, `nextSeq`,
`running`, `pilot: true` and `role: "pilot"` in response `_meta.hive`. Default
resume takes pilot control. `_meta.hive.mode: "observe"` attaches read-only.
`_meta.hive.mode: "relinquish"` requires the pilot's generation/token and makes
it an observer, leaving no pilot until explicit takeover. Both modes optionally
replay retained events before returning. Observer responses have no token.

No implicit new prompt is sent. TUI `--session hive-…` and browser attachment
use this same contract. The browser initially requests `mode: "observe"`
and only attempts pilot resume when the user types a prompt. A pending
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

The pilot resume response returns a new generation/token. Use those for subsequent
commands. `session/cancel` and `session/close` carry the same ownership fields
without a prompt key. Tokens are local control credentials: do not forward
them to downstream agents or print them in client logs.

To observe without taking control, send the same resume request with
`"_meta":{"hive":{"mode":"observe"}}`. The pilot can relinquish without
disconnecting by sending `"_meta":{"hive":{"mode":"relinquish", "generation":N,
"token":"opaque"}}` in `session/resume`. Both receive live updates; another
client sends default `session/resume` to become pilot. Session-scoped permission
and elicitation requests are addressed to the current pilot, and are
re-presented after takeover if still pending and supported by that client.

## Interaction capabilities and fallback

The minimum Hive client contract is ACPv2 session support, permission requests,
response correlation, and request cancellation. A permission UI must show the
agent's title, optional description/subject, and supplied options. It returns
the exact selected option ID or `cancelled`. No missing UI, unknown outcome,
unsupported payload, error, or timeout authorizes an operation. A proxy retains
future outcomes without interpreting them as approval.

Elicitation is optional. Advertise `capabilities.elicitation.form: {}` only when
forms can be presented and valid content returned. Advertise `url: {}` only
when URL consent and `elicitation/complete` notifications can be handled.
Omitted/null modes are unsupported. Hive's TUI and Web UI
advertise both modes. Hive advertises its stable broker-supported modes to
agents, retaining each upstream client's negotiated support separately.

Takeover remains available to less capable clients: an interaction in a mode
the current pilot does not advertise receives `action: "cancel"`, rather than
being delivered to an absent UI. Observing a session requires no optional mode.

Concurrent interactions retain independent RPC identities and queue without
overwriting earlier questions. JSON-RPC IDs are connection-local: Hive allocates
IDs on each hop and correlates responses and `$/cancel_request` notifications.
It does not copy cancellation IDs between transports.

Session cancellation
withdraws all affected pending decisions. Disconnect permits takeover within
the original deadline. Close or worker termination clears pending waits. Old
pilot responses cannot authorize a new generation. Queues are bounded at 128.
Overflow cancels safely.

Native v2 `session/new` and dispatched `session/prompt` requests provide live
parent-ID mappings for request-scoped elicitation. These requests return to
their originating connection even after pilot transfer. Parent completion or
cancellation ends child waits. Repeated logical prompts with multiple RPC
waiters have no unique parent.

Downstream initialization, autonomous recovery,
and unmapped/expired parents are safely cancelled with the diagnostic log code
`hive::interactions::unmapped_parent`. Use session-scoped elicitation or a live
mapped request instead. The v1 adapter supports session-scoped elicitation.

### Form validation

Forms use ACP primitive schemas, including required fields, choices/titled
choices, booleans, numeric bounds, string length/pattern constraints, and
multiselect item counts. Invalid input stays editable. Unknown field types are
not treated as strings. Defaults are suggestions, never automatic submissions.
Formats are annotations, including unknown/future formats.

To keep validation consistent across hosts,
patterns use the common ASCII Rust/ECMAScript subset without escapes,
lookaround, character-set algebra, or dot wildcards. Patterned values containing line separators are
rejected. Unsupported patterns cancel the form. The browser rejects integers
outside JavaScript's safe integer range instead of rounding them.

Client controls are documented in the [TUI](guides/tui.md#answer-agent-questions)
and [Web UI](guides/web.md) guides. Forms must remain
editable after validation errors rather than implicitly submitting defaults.

### URL completion

URL links require explicit user action and HTTP(S). Continue/Enter consents to
proceed. It does not claim that authentication succeeded.

Completion is reported
only on `elicitation/complete`, including if it arrives before the create
response. Hive translates worker-local elicitation IDs to avoid collisions on
a shared upstream connection, preserves completion metadata, and routes to the
actual recipients.

Accepted flows are not restarted on takeover. Completion
records expire after 120 seconds or session cancellation/termination.

These guarantees are exercised by scripted protocol peers and client tests.
They do not upgrade the real-agent compatibility matrix above.

## Mutation ownership and idempotency

Every upstream `session/prompt` and `session/close` request and `session/cancel`
notification needs `_meta.hive.generation` and `_meta.hive.token` from the
latest successful new/resume response. Missing or stale owners receive an
actionable error for requests. Cancel is a JSON-RPC notification, so a stale
cancel is discarded and cannot carry an error response. A subsequent resume
rotates ownership even when the old connection is still alive. A disconnected
pilot relinquishes control. Another client must explicitly take over.

All
attached clients receive live updates, including a sequenced `_hive_control`
event with `generation` and pilot presence but no token. `session/list` includes
profile, running, generation, pilot presence and observer count in each entry's
`_meta.hive`, along with the shared `title`, `lifecycle`, `activity`,
`lastActivityAt` and `controlOwner` summary fields. The owner is an ephemeral
connection ID, **never** the token. An absent title/timestamp/owner is `null`.
`activity: "unknown"` means no trustworthy foreground status is available.
The HTTP catalog and its SSE invalidation feed are documented in
[Hive's operational API](hive-api.md#session-catalog). ACP `session/list` and
HTTP snapshots derive their fields from the same broker.

A close is explicit and cannot be issued while work is active.
client detach does not close the session or cancel the agent. A second distinct prompt
is rejected until the agent sends `idle` (not merely until its prompt response).

Prompts additionally require `_meta.hive.key`, a nonempty client-generated
idempotency key scoped to the Hive session. Retrying the *same key and content*
after an uncertain acknowledgement returns or waits for the original result,
including after takeover. The same key with changed content fails.

A rejected prompt that was never dispatched retains its error under that key.
Use a new key for a newly intended prompt. Do not blindly send a new key after
losing an acknowledgement. Reconnect and retry the existing key while this daemon
is alive.

Reservations and confirmed responses survive restart. An interrupted
key remains reserved and is never redispatched. After inspecting the agent's
state, explicitly take control with `_meta.hive.recoveryAction: "acknowledge"`
on `session/resume`. This permits *new* keys, but does not establish that
previous work completed or allow the interrupted key to be reused. Observe
mode remains available without acknowledgement.

Hive's durable journal stores a versioned SHA-256 fingerprint of decoded prompt
content, not the prompt body. The same comparison applies live and after restart:
object-member order is ignored recursively, while array order, exact text,
content-level metadata, annotations and media/resource payloads participate.
Serialized numeric distinctions, including signed annotation zero, are preserved.
Request-level control credentials and keys are not part of the content hash.

Durable successful outcomes retain only `messageId`; arbitrary response `_meta`
is not persisted. A matching retry after restart reconstructs that minimal
acknowledgement. Failed requests retain a failure marker and return a controlled
previous-failure diagnostic rather than the original downstream error text.
Normal live acknowledgements and in-memory retries may still include protocol
metadata. Schema 2 migrates earlier journal rows without releasing pending keys;
see [storage and backups](operations.md#storage-and-backups).

## Replay cursors

Live `session/update` events carry `_meta.hive.seq`, an increasing per-session
integer, unchanged on incremental replay. On `session/resume`, omit `replayFrom` to attach
without history. `{ "type": "start" }` requests a materialized transcript snapshot.
`{ "type": "_hive_seq", "seq": N }` requests updates **at or after** N,
inclusive. Send `lastProcessedSeq + 1` to avoid duplicates. `nextSeq` in the
resume response identifies the first event not yet produced at the atomic
takeover boundary.

For incremental replay, the most recent 256 updates (including chunks and state
transitions) are available. An expired or future cursor fails with the current
available range and leaves the old controller in place. Live updates are
ordered after replay. A partially delivered replay must be retried using the
last actually processed sequence.

Full transcript snapshots are independent of this bounded ring. They contain
full message/tool upserts in first-appearance order. Snapshot notifications carry
`_meta.hive.snapshot: true` and share a boundary sequence; they are not contiguous
incremental events. The resume response's `_meta.hive.snapshotThroughSeq` gives
that boundary. Stage marked entries until the response succeeds, replace the
conversation view, set the incremental cursor to this boundary, then process
queued ordinary events above it. Do not append ring chunks to the snapshot or
advance past takeover events emitted after snapshot capture. Current activity,
usage, configuration and running state come from the attachment response.

The WebUI rebuilds from `start` on reload
and retains the cursor alongside rendered history only while the page is
alive. An expired cursor requires a visible attach-without-history choice.

The materialized transcript is in memory only and grows with conversation
content; adjacent text chunks are compacted. On daemon restart it is rebuilt
from the downstream agent's retained history. Historical import does not repeat
usage accounting, dispatch interactions, or issue prompts. V1 synthetic message
IDs group consecutive chunks by speaker; v1 cannot provide exact native message
identities or distinguish consecutive same-speaker messages without a boundary.
Hive cannot reconstruct content the agent no longer retains, and a successful
ACPv2 `start` response only establishes replay of the agent's retained history.

The shared `transcript` object in new/resume `_meta.hive`, session summaries and
`_hive_control` updates identifies agent ownership, projection source, restoration
support, availability, unknown/partial completeness and controlled warnings. See
[the status fields](hive-api.md#transcript-ownership-and-status). `transcriptSeq`
in attachment metadata fences older replayed control status. Stage status with
the successful attachment and accept subsequent control status at or above that
watermark. Available partial projections render without live-only acknowledgement.
Context-only recovery initially has no earlier snapshot; newly observed content
can form an available partial snapshot. A fresh session's volatile snapshot works
even if the downstream cannot reconstruct it after restart.

Incremental replay does not survive a daemon restart. Prompt deduplication records do.
Cursor blocks are reserved on disk before publishing updates: a restart can
skip numbers, and an expired cursor fails rather than silently replaying an
incomplete history.

Updates can arrive before prompt responses. `idle` ends foreground work, not a
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
is null. The session snapshot replaces the previous one. `turn.id` is Hive's
idempotency key, not a provider request ID. A later price lookup can publish
a replacement event for the same turn ID. Clients replace rather than sum it.

Unreported/partially reported fields are null, including aggregate categories
when any contributing turn lacks that field. `totalTokens` is the agent's
reported total, not a sum inferred from other categories.

Cost numbers are USD unless the agent supplies a different currency in `actualSessionCost`.
Actual cumulative cost takes precedence over estimates. An estimate includes
`input`, `output`, `cacheRead`, `cacheWrite`, `total`, `currency`, `provider`,
`model`, `retrievedAt` (Unix milliseconds) and `source` (models.dev URL).
Partial estimates have a null total. No price identity or pricing lookup means
null estimated cost, while tokens remain available.

`_meta.hive.usageSnapshot` contains the latest session aggregate atomically on
new/resume. `_meta.hive.usageSeq` marks its event cursor. Summaries from
`session/list` and the HTTP catalog also include `usage`. The HTTP catalog
includes `usageByAgent`, grouped by configured Hive profile across sessions.
An aggregate cost is null if any contributing price is missing or currencies
disagree.

Accounting survives daemon restarts, but the bounded event replay remains
in-memory. A prompt interrupted before idle has no recorded turn. Do not infer
usage from it.

## Pi

**Pi has no native ACP mode.** Pi 1.3.0 offers `--mode text|json|rpc`. `rpc` is
Pi's own JSON protocol, not ACP, and neither its documentation nor its changelog
mentions ACP. Do not assume any Pi version exposes ACP by default.

The setup guide is [Pi integration](agents/pi.md). Hive's supported path is the external adapter
[`pi-acp`](https://github.com/svkozak/pi-acp), which speaks ACP over stdio and
launches `pi --mode rpc` itself. It is a **prerequisite you install**. Hive does
not ship or maintain it. Pin the version you tested:

```toml
[agents.pi]
name = "Pi"
command = "npx"
args = ["-y", "pi-acp@0.0.34"] # or: command = "pi-acp" after `npm install -g pi-acp`
versions = ["v1"]
```

| Item | Value |
| --- | --- |
| Tested | Pi 1.3.0, `pi-acp` 0.0.34, Node 26 (2026-10-23) |
| Minimum | Node 22+ and Pi 0.81.0+, as stated by `pi-acp`; Hive has not tested below the versions above |
| Wire | ACPv1 over stdio. A v2 `initialize` is answered with `protocolVersion: 1`, so `versions = ["v2"]` fails |
| Permissions | Pi runs tools itself, so it never asks Hive for permission; none are required of Hive |
| Authentication | `pi-acp --terminal-login`, or configure credentials in Pi directly |

`initialize` reports (probe output, 2026-10-23): `load_session=true list=true
resume=false image=true audio=false embedded_context=false auth_methods=1`.
Embedded context needs `PI_ACP_ENABLE_EMBEDDED_CONTEXT=true` in the profile's
`env`.

Limitations documented by `pi-acp` (not independently tested by Hive): no
`fs/*` or `terminal/*` delegation, MCP servers are accepted but not passed to
Pi (and Hive's v1 adapter rejects non-empty MCP declarations), and Pi extension
slash commands are unavailable. The adapter calls itself an MVP, so re-run the
probe after upgrading either Pi or the adapter.

Only `initialize` is verified. Prompt turns, streaming, cancellation, tool calls
and reconnect stay **Unverified** in the matrix until someone records a completed
`pi_creates_prompts_and_reattaches_to_a_live_hive_session` run with its versions.
`pi-acp` stores a map of ACP to Pi session IDs, so it can load a session created in
Pi. Whether Hive can adopt such a session is tracked separately in
[#6](https://github.com/noirbizarre/hive/issues/6) and is **not** part of basic
ACP operation.

### Diagnostics

`hive_daemon::compat` checks launch settings for probes and reports actionable
diagnostics. Session startup follows a separate worker path and can report
different errors/timeouts. Codes are public identifiers and never renamed.

| Code | Meaning and fix |
| --- | --- |
| `hive::compat::binary_missing` | The command is not on `PATH`. Install it (for Pi: `npm install -g pi-acp`) or fix `command`. |
| `hive::compat::pi_without_adapter` | The command is `pi` with no ACP argument, which starts Pi's own interface. Use `pi-acp`. |
| `hive::compat::protocol_mismatch` | The agent negotiated another ACP version than the profile declares. Fix `versions`. |
| `hive::compat::missing_capability` | The agent omits a capability Hive needs, such as v2 `session`. |
| `hive::compat::startup_timeout` | No `initialize` answer within 30 seconds. Run the command by hand. |
| `hive::compat::initialize_failed` | The handshake failed (wrong protocol, crash, not ACP). Check the agent's stderr. |
| `hive::compat::not_probeable` | The stdio probe needs `command`; endpoint profiles use the endpoint probe. |
| `hive::compat::endpoint_unreachable` | Nothing accepted a TCP connection at the profile's `endpoint`. The URL and headers are never printed. |

### `hive doctor`

`hive doctor [AGENT]...` runs these checks for every profile (or only the ones
named) and prints the diagnostics above with their help text:

- **Configuration**: every problem at once, not only the first.
- **stdio profiles**: the launch checks and the initialize-only probe, with the
  resulting capability report. For `pi-acp` (or `npx -y pi-acp`) it also warns
  when Node.js 22+ or `pi` is missing from `PATH`.
- **Endpoint profiles**: a TCP connect, then the initialize-only probe at the
  declared protocol version.
- **`opencode-server` profiles**: the health and version probes, with
  `hive::opencode::*` codes.
- **Daemon**: the listen address (loopback only), whether another daemon holds
  the lock (and answers there), and the permissions of the state directory.
  Doctor never creates the lock or state files.

It exits with status 1 when any check fails. Warnings alone exit 0. `--json`
prints the same report as one JSON object (`ok`, `config`, `profiles`,
`daemon`), each finding having `status`, `code`, `message` and `help`.

Nothing sends a prompt. Reports never contain endpoint URLs, headers, environment
values, arguments or agent output. Agent names and versions are reduced to
printable characters. A missing-binary message does name the configured
`command`, so review a pasted report before sharing it.

| Code | Meaning and fix |
| --- | --- |
| `hive::config::unreadable` | The config file cannot be read. Check the path and permissions. |
| `hive::config::syntax` | The TOML is invalid or has an unknown field; only the line is reported. |
| `hive::config::default_agent` | `default_agent` is not one of the `[agents]` ids. |
| `hive::config::agent_id` | The profile id uses characters other than letters, digits, `-` and `_`. |
| `hive::config::pricing` | The `pricing` table is incomplete. |
| `hive::config::versions` | The profile declares no protocol version. |
| `hive::config::profile` | The profile needs a stdio `command` or an `endpoint`, not both or neither. |
| `hive::config::endpoint` | Endpoint is missing/malformed. OpenCode server profiles also reject URL credentials and remote plain HTTP. |
| `hive::config::header` | A header name or value is invalid (neither is printed). |
| `hive::config::credentials` | `password_env` is not a variable name, or credentials fields are set on an ACP profile. |
| `hive::doctor::unknown_agent` | A named profile is not configured. |
| `hive::doctor::no_agents` | No profile is configured (warning). |
| `hive::doctor::missing_prerequisite` | Node.js 22+ or `pi` needed by `pi-acp` was not found (warning). |
| `hive::doctor::listen_not_loopback` | The daemon refuses non-loopback listen addresses. |
| `hive::doctor::port_in_use` | Another program holds the listen address. |
| `hive::doctor::daemon_locked` | Another daemon holds the lock but does not answer on the listen address (warning). |
| `hive::doctor::no_runtime_dir` | Neither `XDG_RUNTIME_DIR` nor `HOME` is set, so the lock cannot be located (warning). |
| `hive::doctor::no_state_dir` | Neither `XDG_STATE_HOME` nor `HOME` is set, so the state directory cannot be located (warning). |
| `hive::doctor::probe_panicked` | A probe stopped unexpectedly; re-run with `RUST_BACKTRACE=1` and report a bug. |
| `hive::doctor::unsafe_runtime_dir` | The runtime directory or lock is not private; the daemon would refuse to start. |
| `hive::doctor::unsafe_state_dir` | State directory/database failed doctor's privacy or ownership checks; inspect actual startup separately. |

Run the free, opt-in probe against a real Pi (it sends no prompt) with:

```sh
HIVE_REAL_PI=1 cargo test -p hive-daemon --test real_agents \
  pi_initialize_probe_reports_a_capability_report -- --ignored --exact --nocapture
```

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
| `external` | `active`, `source` | A native OpenCode client, not Hive, started (`true`) or ended (`false`) foreground work on an adopted session. Hive prompts are refused while it is active. |
| `connection` | `status`, `source` | The adapter's link to the OpenCode event stream: `lost`, `restored` or `failed` (given up; the session becomes unavailable). |

`source` is `acp-v1`, `acp-v2`, `acp` (v1/v2 tool patches), or `extension`.
`_meta.hive.seq` is the ordinary per-session event cursor, including normalized
updates.

Tool patches follow these rules:

- Absent fields retain their previous value.
- Explicit null clears a field.
- Arrays replace previous arrays.
- Content chunks append until a full content update replaces them.
- Top-level provider `_meta` is excluded from tool/plan/task projections.

IDs are scoped to the Hive session.
Turn IDs, parent task IDs, and other relationships are present only when the
downstream event provides them. A tool or task update after idle never reopens
the foreground turn or closes the session.

`_meta.hive.activitySnapshot` in new/resume responses contains `{version:1,
seq,session,foreground,tools,plans,tasks,support}` at the atomic replay/live attachment
boundary. `seq` is the last normalized activity event. `nextSeq` remains the
first not-yet-produced *session update*.

The support values for tools, plans,
and tasks are `observed`, `unknown`, or `unsupported`. V1 task identities are
unsupported by Hive's stable v1 adapter. Otherwise an absent todo update cannot
justify an invented plan or an unsupported claim. Unrecognized provider statuses
are preserved.

Finished tools are retained up to 128 per session. Streaming chunk snapshots
retain the latest 64 chunks and
flag `chunksTruncated` when earlier chunks have been evicted. Raw event history
remains bounded at 256 updates (now including normalized activity), so an
incremental cursor can expire sooner than before. Full transcript snapshots
are independent of these activity and ring limits.

Apply replay in sequence order and ignore repeated sequence numbers. Identical
downstream content chunks without provider event IDs cannot be distinguished
from legitimate repeated chunks. Entity upserts still replace the same call or plan.

On a gap, replay from the last applied sequence plus one. If that cursor has expired,
attach live-only after an explicit history-gap decision, restore
`activitySnapshot`, and continue from `nextSeq`. A snapshot restores *current
activity*, not missed conversation text.

Buffer updates that arrive before the
resume response and reduce them after the snapshot so later activity cannot be
overwritten. A daemon restart loses both history and this in-memory snapshot.
Activity resumes as unknown rather than assuming the interrupted turn was idle.
V1 tool calls and plans are adapted. ACPv2 provides tool, plan, and foreground
updates natively. Background tasks require explicit provider extension events.
Neither wire version guarantees them.

## OpenCode server adapter

A profile with `kind = "opencode-server"` talks to a running OpenCode server
(`opencode serve`, or the TUI started with `--port`) over its REST + SSE API
instead of spawning an ACP process. Clients still speak ACPv2. The decision and
its trade-offs are recorded in [ADR-002](adr/002-opencode-server-adoption.md).

```toml
[agents.opencode-server]
kind = "opencode-server"
endpoint = "http://127.0.0.1:4096"
username = "opencode"                    # optional; this is OpenCode's default
password_env = "OPENCODE_SERVER_PASSWORD" # name of a variable, never the secret
```

Adopt a native session in two steps. List candidates (the `directory` scopes
OpenCode's project lookup and should be the session's working directory), then
send `session/new` with the native ID:

```sh
curl 'http://127.0.0.1:PORT/v1/providers/opencode-server/native-sessions?directory=/work/app'
```

```json
{"cwd":"/work/app","mcpServers":[],"_meta":{"hive":{"agent":"opencode-server","adopt":{"nativeId":"ses_…"}}}}
```

Adoption attaches to the existing session and never creates one. The response
and catalog carry a stable Hive session ID. The catalog also exposes
`origin` (`adopted`, or `hive` for sessions Hive created), `nativeId` and
`externalActivity`.

A session Hive creates on this profile is a real OpenCode
session: resume it there with `opencode attach <url> -s <nativeId>`. Closing a
Hive session only detaches. It never deletes the OpenCode session, and the same
native session can be adopted again afterwards.

Both `agent` and `adopt` are required in this request. Duplicate protection is
scoped to the profile/native ID, not globally across profiles pointing to the
same server. See the [OpenCode guide](agents/opencode.md) for the user workflow.

Up to the 50 most recent native messages are imported into the session's
transcript projection (not pushed live to the adopting client): resume with
`replayFrom: {"type":"start"}` to read them. Older history stays in OpenCode.

### Capability and version matrix

Evidence: the opt-in `real_opencode_server` probe, run on 2026-10-23 against a
throwaway `opencode serve` (temporary XDG directories, free
`opencode/big-pickle` model), plus the deterministic fake-server suite in
`crates/hive-daemon/tests/opencode.rs`. `Guaranteed` is limited to what the
retained probe demonstrated; "fake only" means the behavior is covered by the
fake server but not yet by a real OpenCode run.

| OpenCode | API used | Discover | Adopt without a duplicate | Text prompt, stream, idle | Continue native context | Resume in OpenCode |
| --- | --- | --- | --- | --- | --- | --- |
| 1.19.3 | v1 routes | Guaranteed | Guaranteed | best-effort (model dependent) | Guaranteed | Guaranteed |
| 1.19.3 | `/api/*` v2 preview | Fake only | Fake only | Fake only | Fake only | Unverified |
| below 1.19.0 | any | unsupported: `hive::opencode::unsupported_version` | unsupported | unsupported | unsupported | unsupported |

| OpenCode | Cancel | Permissions | Questions | Stream drop and reconnect | Restart recovery | External client detected |
| --- | --- | --- | --- | --- | --- | --- |
| 1.19.3 v1 | Fake only | Fake only | Fake only | Fake only | Fake only | Fake only |
| 1.19.3 v2 preview | Fake only | Fake only (polled) | unsupported | Fake only | Fake only | Fake only |

The probe printed these markers (retain them when changing a cell):
`PROBE server=opencode version=1.19.3 api=v1`, `NATIVE discovered=true`,
`ADOPTED … native_session_reused=true`, `FIRST text_observed=true
recalled_context=true`, `RESUMED duplicate_session=false native_messages=4`.

Versions below 1.19.0, and servers answering neither `/global/health` nor
`/api/health` with a valid JSON body (an HTML catch-all is not mistaken for an
API), fail with an actionable diagnostic rather than creating a session. The
minimum is a compatibility policy, not proof that every older server is
incapable. Reported versions below it are rejected rather than tried.

### API flavors

OpenCode 1.19.3 serves two session APIs with **separate message stores**. Hive
prefers the documented v1 routes, which the TUI uses, and falls back to the
`/api/*` preview only when v1 is unavailable or does not know the session. The
choice is made once per session and saved: recovery uses the same flavor or
fails with `hive::opencode::api_unavailable`. It never switches.

The v1 event
stream has no replay cursor, so after a dropped stream Hive re-reads status and
recent history and de-duplicates by part, whereas the v2 stream resumes from its
durable sequence number.

The v2 preview does not support questions, and its
permission requests are polled rather than pushed.

### Single controller

OpenCode does not arbitrate between clients: any client can prompt, abort or
answer permissions at any time, and Hive cannot lock them out. Among Hive
clients the usual pilot/observer fencing applies. Against native clients Hive
detects and refuses:

- A turn Hive did not start (the session goes busy with no Hive prompt in
  flight) sets `externalActivity`, publishes `running` and an `external`
  activity event, and makes Hive prompts fail with
  `hive::opencode::externally_controlled` until the session is idle. The pilot
  may still cancel it.
- A permission or question answered in OpenCode first is withdrawn from Hive's
  pilot and never answered twice. One left unanswered (timeout or cancel) stays
  pending in OpenCode, where a native client can still answer it, and is offered
  to Hive again.

### OpenCode diagnostics

Failures carry a stable code, what failed and what to do. They never include the
endpoint, headers, credentials or server response bodies.

| Code | Meaning |
| --- | --- |
| `hive::opencode::unreachable` | The server did not answer or the connection failed. Recovery retries this. |
| `hive::opencode::unauthorized` | Credentials were rejected. |
| `hive::opencode::missing_credentials` | `password_env` names a variable that is not set. |
| `hive::opencode::unsupported_version` | Older than the tested minimum. |
| `hive::opencode::api_unavailable` | A required API is not offered. The session is `orphaned`. |
| `hive::opencode::session_not_found` | The native session is gone. The session is `lost`. |
| `hive::opencode::directory_mismatch` | The session belongs to a different project than the requested `cwd`. |
| `hive::opencode::already_adopted` | Carries `hiveSessionId` of the existing session. |
| `hive::opencode::adoption_in_progress` | Another request is adopting the same session. |
| `hive::opencode::externally_controlled` | A native client is driving the session. |
| `hive::opencode::stream_lost` | The event stream dropped. Recovery retries this. |
| `hive::opencode::unsupported_prompt` | Only text prompt content is forwarded. |
| `hive::opencode::http_status` | An unexpected HTTP status. |
| `hive::opencode::protocol` | A response did not have the expected shape. |

### Security and unsupported workflows

- `opencode serve` is **unauthenticated** unless `OPENCODE_SERVER_PASSWORD` is
  set on the server. Set it, and give Hive the variable *name* in `password_env`.
  Profile validation refuses plain `http://` to a non-loopback host and
  credentials embedded in the endpoint; use `https://` or an SSH tunnel.
- Anyone who can reach Hive's own (loopback) gateway can act through its
  OpenCode credentials, and the native-sessions route reveals session titles and
  directories. The existing single-user loopback policy applies.

Unsupported workflows:

- Simultaneous control from OpenCode and Hive (detected, not prevented).
- Deleting or forking native sessions through Hive.
- Unifying v1 and preview message stores.
- Backfill older than the 50-message window.
- MCP declarations in `session/new` or non-text prompts.
- Questions over the preview API.

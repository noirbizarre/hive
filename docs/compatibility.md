# ACP / ACPv2 compatibility matrix

This is the protocol and verification reference. For setup, start with
[getting started](getting-started.md), [configuration](configuration.md), or
the [OpenCode](agents/opencode.md), [Claude Code](agents/claude.md),
[Codex](agents/codex.md), and [Pi](agents/pi.md) guides.

## Audit baseline

Re-audited **2026-10-26** for the SDK 3.5.0 upgrade, with pre-upgrade behavior
checked against Hive commit
[`f9e6a41`](https://github.com/noirbizarre/hive/tree/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17).
Upgrade-specific regression names below refer to the accompanying source changes.
The matrix inventories the specification shipped with Hive's current dependencies,
including features Hive does not implement. It is not a declaration of full ACP conformance.

| Version identifier | Audited value |
| --- | --- |
| Wire protocols | ACPv1 downstream compatibility; **draft ACPv2** upstream and native downstream |
| Rust SDK / HTTP adapter | `agent-client-protocol =3.5.0` / `agent-client-protocol-http =3.5.0` |
| Rust schema crate | `agent-client-protocol-schema =1.12.0`, resolved in `Cargo.lock` |
| Schema/specification provenance | [`e2d83349bc9e28a4059ea3acefa1c91c966f74e2`][spec-pin], 2026-10-16, from the published crate's `.cargo_vcs_info.json` |
| SDK / HTTP adapter provenance | [`305aefda6fbe88be6ae8b9088003e383f909b135`][sdk-pin], independently confirmed from both published crates' `.cargo_vcs_info.json` |
| Additional upstream draft reviewed | [`1c0fe8dd680aa6de119b2136a7e5495075baf257`][draft-pin], 2026-10-26; not the published schema's source |
| JSON schema artifact versions at the schema source revision | v1 **1.26.0**; v2 **2.0.0-alpha.9**; these are not Rust crate or wire versions |
| Enabled experimental protocol features | `unstable_protocol_v2`, `unstable_end_turn_token_usage` |

The authoritative inventory is the pinned [v1 registry][v1-methods] and [schema][v1-schema],
[v2 registry][v2-methods] and [schema][v2-schema], and their
[v1][v1-unstable] / [v2][v2-unstable] experimental additions.
Here, **baseline v2** means the schema without additional experimental features;
the whole v2 protocol is still a draft. A feature appearing in an SDK type or an
experimental schema does not establish that Hive enables or supports it.

The latest suitable published SDK and HTTP releases are 3.5.0; both still use
schema 1.12.0. A source override is unnecessary for the enabled features. This
audit also reviews the [complete schema-to-draft comparison][draft-comparison],
including the intermediate `66ff8076` revision requested in
[#64](https://github.com/noirbizarre/hive/issues/64). The following differences
qualify the dependency-matched matrix; equal manifest/schema artifact versions
do not imply equal source or full conformance to the newer draft.

### Reviewed draft differences

| Changed surface at the reviewed draft | Hive behavior / deviation |
| --- | --- |
| Version selection and errors ([draft initialization][draft-init]) | SDK 3.5.0 selects the highest configured version at or below the request. Hive's v2-only endpoint selects v2 for compatible newer requests, rejects v1 with `-32022` and required `data.requested` / `data.supported`, and rejects missing/non-integer/out-of-range versions with `-32602`. Errors leave the connection uninitialized. The SDK implements this even though schema 1.12.0 lacks the named error variant. |
| Lower-version successful initialization ([SDK v2 guide][sdk-v2]) | Downstream v2 probes/workers use typed `send_initialize` outcomes to report a validated v1 selection as a profile mismatch. Hive never switches adapters or retries automatically. Explicitly configured v1 adaptation remains available. |
| Available-command `type` discriminator and custom/future kinds ([draft commands][draft-commands]) | The newer v2 schema requires `type: "command"` or a custom/future discriminator and raw preservation for unfamiliar command metadata. Released schema 1.12.0 still models the older untagged command shape; its typed round-trip drops the discriminator and unmodeled fields. Native command relay is therefore not conformance to the newer shape. Neither built-in client offers command discovery/completion. |
| Inter-session message identity ([draft experimental schema][draft-unstable]) | `session_message` upserts add `senderMessageId` / `recipientMessageId` to link transcript-local views. Participants/references belong on upserts; chunks carry content, not identity. `unstable_subagents` stays disabled; generic unknown-update relay does not implement linkage, child routing, or control. |
| Resume synchronization ([draft session setup][draft-setup]) | The draft additionally recommends current config in the response and current `session_info_update` after replay, before response, even without replay. Hive returns current config snapshots and a fresh standard state barrier when foreground state is known; a standard session-info snapshot remains missing. |
| Configuration and session-info notifications ([draft configuration][draft-config], [draft session list][draft-list]) | Agents should notify attached clients after changes, including client setters; duplicate notifications should precede the setter response, subsequent changes follow it. Hive publishes confirmed config changes to subscribers and reconciles newer observations; standard session-info retention remains partial. |
| Adjacent streamed tool text ([draft tools][draft-tools]) | Each text piece may be a separate `tool_call_content_chunk`; adjacent items can be displayed continuously. Existing typed relay/materialization accepts these chunks. Rich presentation and the superseded activity-buffer limitation remain as listed below. |
| Session status and client-set titles RFDs | `session/status` and `session/set_title` are draft proposals, absent from both reviewed method registries; their publication does not add supported methods or capabilities to Hive. |

SDK 3.5.0 also supplies a termination signal for every connection end. Independent
gateway cleanup observers wait for EOF **or** termination so failed/dropped drivers
release subscriptions, ownership, and connection capabilities without killing the
downstream session. The TUI foreground still waits on EOF: driver termination is
published after foreground completion and cannot replace that wakeup.

Upgrade regressions in the accompanying
`crates/hive-daemon/tests/gateway.rs` and `crates/hive-daemon/src/lib.rs` include
`gateway_negotiation_errors_and_future_versions_match_the_draft_over_both_transports`,
`dropping_the_gateway_driver_releases_capabilities_without_waiting_for_eof`, and
`driver_drop_releases_only_its_attachment_and_fences_a_pilot_once`.
The TUI connection tests in `crates/hive-tui/src/lib.rs` include
`the_tui_foreground_returns_on_eof_without_waiting_for_driver_termination`.

## Reading the matrix

| Status | Meaning |
| --- | --- |
| **✅ S — Implemented and supported** | The stated Hive behavior has implementation/test evidence. It does not promise that every agent emits the feature. |
| **🟡 P — Partially supported** | Some layers or protocol semantics are missing; the row names the limitation. |
| **🔀 A — Agent-dependent / pass-through** | Hive relays/delegates the stated surface; availability and actual behavior depend on the selected agent. |
| **📋 Planned** | A committed future feature, with its tracking issue. A gap issue alone does not establish this status. |
| **🚫 Not planned** | An explicit product decision not to support a feature; requires a policy reference. |
| **❔ Not assessed / unknown** | The audit has insufficient evidence; never inferred from agent silence. |
| **❌ U — Currently unsupported** | No usable Hive implementation for this surface. This additional status describes the present, not roadmap intent. |

No optional feature is assigned **Planned** or **Not planned** without evidence of that
decision. **Not applicable** in a layer means the feature belongs elsewhere, not unknown support.
Icons supplement the letters and labels, so meaning does not depend on color alone.
The [Known agents](#known-agents) tables below use a different, empirical evidence legend.

The status column describes the feature across the listed layers. **Ingestion** separates
the downstream v1 adapter from native v2. **Daemon / API** includes session state,
capability advertisement, relay to external clients, and replay. **TUI / Web** names
built-in client behavior; API relay alone is not end-to-end UI support.
All client-facing ACP is v2; v1-only methods are not available directly upstream.
Evidence links point to the audited Hive revision; named tests identify the exercised
behavior, not a claim that live product probes covered every row.

## Standard feature inventory

### Initialization, authentication, and sessions

Sources: [v1 initialization][v1-init], [v2 initialization][v2-init],
[v1 session setup][v1-setup], [v2 session setup][v2-setup], and the method registries above.

| Feature / specification | Status | Ingestion: v1 / v2 | Daemon / client API | TUI / Web | Evidence / gap |
| --- | --- | --- | --- | --- | --- |
| `initialize`, version and implementation identity ([v1][v1-init], [v2][v2-init]) | ✅ S | Explicit profile selects one version/transport; wrong negotiation fails | Upstream v2; advertises Hive's own session surface, not merged agent capabilities | Both initialize as v2 | [Workers/gateway][daemon]; [compatibility probes][compat-tests] |
| Optional capability negotiation ([v1][v1-init], [v2][v2-init]) | 🟡 P | Native v2 requires session support; v1 restoration checks load/resume | Upstream only `session: {}`; optional agent flags are not exposed as a per-session capability catalog | Both negotiate elicitation; other optional features have the limits below | [Workers/gateway][daemon]; [interaction broker][interactions] |
| v1 `authenticate`, `logout`; v2 `auth/login`, `auth/logout`, `authMethods` ([v1][v1-auth], [v2][v2-auth]) | ❌ U | No agent login/logout orchestration; doctor can report auth-method count | No auth methods advertised or forwarded; agents must already be authenticated | No ACP login UI; URL elicitation is separate | [Workers/gateway][daemon]; [doctor/agent setup](#pi) |
| Terminal authentication and logout capability ([v1][v1-auth], [v2][v2-auth]) | ❌ U | No terminal-auth handler or credential flow | Neither `auth.terminal` nor logout workflow exposed | Neither client launches an ACP login terminal | [Workers/gateway][daemon] |
| `session/new`, cwd, selected agent ([v1][v1-setup], [v2][v2-setup]) | ✅ S | New downstream session with saved native ID | Stable Hive ID; selection uses a Hive extension, not a standard field | Both create sessions | [Gateway tests][gateway-tests]; [protocol boundary](#protocol-boundary) |
| `session/list`, cwd filter, pagination ([v1][v1-list], [v2][v2-list]) | 🟡 P | Does not enumerate arbitrary downstream sessions | Lists entire Hive catalog; cwd filter works; incoming cursor unused, no next cursor; richer summaries in Hive metadata | Both use Hive catalog | [Gateway][daemon]; [session catalog](hive-api.md#session-catalog) |
| v1 `session/load` ([v1 setup][v1-setup]) | 🔀 A | Preferred restoration method when advertised; replays retained history | Used internally after restart; upstream equivalent is v2 resume, not load | Both attach through Hive resume | `earlier_conversations_restore_once_under_the_original_identity_for_both_acp_versions` in [gateway tests][gateway-tests] |
| `session/resume`: live attachment and restart ([v1][v1-setup], [v2][v2-setup]) | 🟡 P | V1 load preferred, resume-only has no earlier history; v2 restores saved identity with `start` | Live worker attachment is broker-owned; never silently creates a replacement; known-state barrier precedes response | Both attach/reconnect with Hive control metadata | [Gateway tests][gateway-tests]; [replay details](#replay-cursors) |
| Current `state_update` after replay, before resume response ([v2 setup][v2-setup]) | 🟡 P | Retains running/idle/requires-action, including restoration evidence | Fresh sequenced barrier after replay for known state, even without replay; unknown attachment succeeds without inventing idle or emitting a barrier | Both consume consistent Hive response state; generic v2 clients receive known-state barriers | `every_resume_replay_mode_ends_with_current_state_before_the_response_on_both_transports` in [gateway tests][gateway-tests]; [unknown-state exception](#replay-cursors) |
| `session/close`, active-work cancellation ([v1][v1-setup], [v2][v2-setup]) | ✅ S | V1 cancels and waits for completion before disconnecting; v2 forwards close concurrently with pending requests | Owner-only; withdraws interactions, cancels configuration and orders final updates/idle before success | Clients can cancel separately; no close UI | [Close lifecycle](#mutation-ownership-and-idempotency); [gateway tests][gateway-tests] |
| `session/delete` and delete capability ([v1][v1-delete], [v2][v2-delete]) | ❌ U | No downstream delete forwarding | Not advertised; close is not deletion | No delete operation | [Gateway handlers][daemon] |
| Additional workspace directories ([v1][v1-setup], [v2][v2-setup]) | 🟡 P | V1 rejects; native v2 new-session roots retained | Saved roots reused for autonomous recovery; incoming upstream resume roots **not applied**; optional support not advertised | No additional-root composer | [Workers/broker][daemon]; [persistence][persistence] |

### Prompts, messages, state, and content

Sources: [v1 prompt turns][v1-prompt], [v2 prompt lifecycle][v2-prompt],
[v1 content][v1-content], [v2 content][v2-content].

| Feature / specification | Status | Ingestion: v1 / v2 | Daemon / client API | TUI / Web | Evidence / gap |
| --- | --- | --- | --- | --- | --- |
| `session/prompt`, acceptance versus completion ([v1][v1-prompt], [v2][v2-prompt]) | ✅ S | V1 completion adapted into asynchronous running/idle; native v2 returns insertion `messageId` | Acknowledgement is not completion; ownership/idempotency and active-foreground admission include blocked-first and restored work | Both consume state, not just response; blocked work remains busy | `blocked_foreground_reserves_admission_until_idle_on_both_transports` in [gateway tests][gateway-tests]; [ownership](#mutation-ownership-and-idempotency) |
| `state_update`: `running`, `idle`, `requires_action`; background updates ([v2 lifecycle][v2-prompt]) | ✅ S | V1 synthesizes running/idle; v2 relays all native states | Admission, catalog, and resume retain blocked state, including restoration; unknown is distinct from idle; late tools do not reopen foreground | Native updates, activity snapshots and normalized foreground agree; blocked work stays busy without an execution spinner | `restored_blocked_sessions_reject_new_work_on_both_transports` in [gateway tests][gateway-tests]; [TUI state][tui-state]; [Web reducer tests][web-tests] |
| Stop reasons: end-turn, limits, refusal, cancellation, v2 error ([v1][v1-prompt], [v2][v2-prompt]) | ✅ S | All known successful v1 stop reasons map to equivalent v2 reasons; future non-equivalent reasons use `_v1_stop`; native v2 keeps typed reason | Native idle/error emits a controlled, materialized `_hive_error` companion; v1 failures emit a controlled extension before idle | Both render failure companions live and on replay; TUI also displays cancellation; Web ends work without inventing a reason | `stop_reasons_and_native_failures_survive_the_gateway_and_replay` in [gateway tests][gateway-tests]; [error boundary](#protocol-boundary) |
| Text blocks; `user_message_chunk`, `agent_message_chunk` ([v1][v1-content], [v2][v2-content]) | ✅ S | V1 synthetic IDs, live user echo suppressed; v2 native IDs/chunks | Relayed; transcript coalesces compatible text chunks in first-appearance order | Both compose/render text streams; Web renders user/agent discussion Markdown, including accumulated chunks | [Transcript][transcript]; [TUI state][tui-state]; [Web reducer tests][web-tests]; [Markdown tests][web-markdown-tests] |
| `user_message`, `agent_message`, `agent_thought`: whole-message upserts and null/omission ([v2 schema][v2-schema]) | ✅ S | V1 has no whole-message identity API; v2 typed updates accepted | Transcript replaces arrays, retains omission, clears null | Both preserve omitted content, clear null/empty arrays, replace concrete content, and append later chunks in first-appearance order | `full_message_patches_replace_content_without_reordering_or_erasing_omitted_fields` in [transcript][transcript]; `message_patches_preserve_omissions_clear_null_and_match_restored_snapshots` in [TUI state][tui-state]; [Web reducer tests][web-tests] |
| `agent_thought_chunk` ([v1][v1-prompt], [v2][v2-prompt]) | 🟡 P | V1 adapter drops thoughts; v2 relays | Native thoughts materialized with a separate message namespace | Both render native thought text | [V1 match][daemon]; [TUI][tui-state]; [Web][web-client] |
| Image and audio blocks / prompt capabilities ([v1][v1-content], [v2][v2-content]) | 🟡 P | V1 prompts rejected; native v2 typed fields retained | Image/audio capabilities not advertised upstream; relay is not supported media negotiation | Text-only composers; placeholders for full-message media; Web non-text chunks have no visible text | [Workers][daemon]; [TUI][tui-state]; [Web][web-client] |
| `resource_link` (baseline prompt type), embedded `resource` text/blob ([v1][v1-content], [v2][v2-content]) | 🟡 P | V1 non-text prompts rejected, including baseline links; native v2 typed content accepted | Embedded-context capability not advertised; no resource-fetching promise | No resource composer/rich rendering; placeholders/generic content | [Workers][daemon]; [TUI rendering][tui-transcript]; [Web][web-client] |
| Content annotations: audience, priority, last-modified; resource icons ([v1][v1-content], [v2][v2-content]) | 🟡 P | V1 prompt text reconstructed without annotations/metadata; native v2 retains modeled fields | Typed content retained/fingerprinted, not an arbitrary JSON preservation guarantee | No annotation-driven presentation or resource icon UI | [Prompt journal][journal]; [transcript][transcript] |

### Tools, terminals, plans, and interactions

Sources: [v1 tool calls][v1-tools], [v2 tool calls][v2-tools],
[v1 plans][v1-plans], [v2 plans][v2-plans], and [elicitation][v2-elicitation].

| Feature / specification | Status | Ingestion: v1 / v2 | Daemon / client API | TUI / Web | Evidence / gap |
| --- | --- | --- | --- | --- | --- |
| V1 `tool_call`, `tool_call_update`; v2 upsert `tool_call_update` ([v1][v1-tools], [v2][v2-tools]) | 🟡 P | V1 extension wrappers; v2 native updates | Raw events plus normalized activity and materialized tools; IDs scoped to session | Both show tool title/status/input/results and preserve omitted fields while clearing explicit nulls; rich presentation remains partial | [Activity][activity]; [transcript][transcript]; [Web reducer tests][web-tests] |
| Tool kinds/statuses/locations, raw input/output, omitted/null patches ([v1][v1-tools], [v2][v2-tools]) | 🟡 P | V2 typed unknown values retained where schema allows; v1 retains wrapped tool payload | Daemon applies omission/null/array replacement; normalized activity strips provider metadata | Both reduce structured patches before display; nonempty content takes precedence over current raw output, with null/empty content allowing raw-output fallback | [TUI state][tui-state]; [TUI renderer][tui-transcript]; [Web reducer tests][web-tests] |
| `tool_call_content_chunk`, embedded content/diff/terminal reference ([v2 tools][v2-tools]) | 🟡 P | Native v2 chunks; v1 tool content remains in wrappers | Transcript appends/replaces canonical content; activity keeps a bounded chunk suffix after the latest content replacement | TUI shows structured details and references; Web readable/JSON output, not full rich parity | [Activity][activity]; [TUI renderer][tui-transcript] |
| Structured v2 diffs: add/delete/modify/move/copy; text/binary/directory/symlink; optional git patch ([v2 tools][v2-tools]) | 🟡 P | Native typed relay, not lossless reverse conversion to v1 text diffs | Preserved inside typed tool content; no filesystem operation executed by receiving a diff | TUI details and Web generic output do not implement every file-change presentation | [Transcript][transcript]; [TUI renderer][tui-transcript]; [Web][web-client] |
| V1 `fs/read_text_file`, `fs/write_text_file` ([filesystem][v1-fs]) | ❌ U | No delegated filesystem handlers; flags not advertised | No upstream equivalent in v2; agents may still operate on files themselves | No delegated filesystem service | [V1 worker initialization][daemon] |
| V1 `terminal/create`, `terminal/output`, `terminal/release`, `terminal/wait_for_exit`, `terminal/kill` ([terminals][v1-terminals]) | ❌ U | No delegated terminal handlers; flag not advertised | These methods were removed from v2; distinct from agent-owned display terminals | No delegated process service | [V1 worker initialization][daemon] |
| V2 `terminal_update`, `terminal_output_chunk` ([v2 tools][v2-tools], [schema][v2-schema]) | 🟡 P | Native typed relay; no v1 equivalent adaptation | Ring replay only; terminal history omitted from materialization and marked limited | Neither renders the display-terminal stream; tool terminal references are separate | `omitted_terminal_history_marks_the_projection_as_limited` in [transcript][transcript]; [client reducers][web-client] |
| V1 flat `plan` ([v1 plans][v1-plans]) | ✅ S | Adapted through `_hive_v1_plan` and activity `v1-plan` | Current activity snapshot; does not establish native v2 plan observation | Both render v1 fallback entries | [Activity][activity]; [native-plan boundary](#native-acpv2-plans) |
| V2 itemized `plan_update`, priorities/statuses, multiple IDs, replacement ([v2 plans][v2-plans]) | ✅ S | Native typed update; no dedicated agent plan capability | Canonical plan snapshot, ring/start replay, metadata/order preservation within SDK boundary | Both render ordered entries and unfamiliar plan content generically | [Plan tests][plans]; `plans_outlive_the_replay_ring_and_restart_when_the_agent_replays_them` in [gateway tests][gateway-tests] |
| `session/request_permission`, exact option IDs, allow/reject once/always ([v1][v1-tools], [v2][v2-tools]) | ✅ S | V1 translated; v2 title/description/optional subject retained | Routed to current pilot; correlation, timeout, cancellation, generation fencing; no implicit approval | Both show supplied options and return exact ID or cancellation | `permission_and_form_and_url_elicitation_reach_the_current_pilot` in [gateway tests][gateway-tests]; [interaction details](#interaction-capabilities-and-fallback) |
| V2 permission subjects: tool-call / command / absent / unfamiliar ([v2 tools][v2-tools]) | 🟡 P | Typed relay preserves modeled/future subject data | Subject not an authorization decision; unknown outcomes never approval | Supplied prompt remains actionable; specialized presentation is not guaranteed for unfamiliar subjects | [Interaction broker][interactions]; [client interaction tests][tui-interactions] |
| `elicitation/create`: form mode, primitive schemas/validation ([v1][v1-elicitation], [v2][v2-elicitation]) | 🟡 P | V1 session-scoped handler exists but initializer does not advertise form; v2 broker advertises form | Pilot capability checked; supported primitive validation; restricted cross-host pattern subset | Both support forms; browser rejects unsafe integers; unsupported patterns/types cancel | [Validation limits](#form-validation); [interaction broker][interactions]; [TUI tests][tui-interactions]; [Web tests][web-component-tests] |
| URL mode and `elicitation/complete` ([v1][v1-elicitation], [v2][v2-elicitation]) | 🟡 P | V1 handlers without mode advertisement; v2 broker advertises URL | IDs remapped, early completion correlated, takeover/deadline behavior defined | Both require explicit HTTP(S) link action/consent; completion is distinct from consent | [URL completion](#url-completion); [gateway tests][gateway-tests]; [Web tests][web-component-tests] |
| Session/request interaction scope; `accept` / `decline` / `cancel` ([v2 elicitation][v2-elicitation]) | 🟡 P | V1 session scope; native v2 live new/prompt parents mapped | Request scope requires a unique live parent; unmapped/expired/ambiguous parents cancel; session scope follows pilot | Both handle queued interactions; unsupported negotiated modes cancel | [Routing limits](#interaction-capabilities-and-fallback); [interaction broker][interactions] |
| OpenCode `plan_exit` confirmation | 🟡 P | Server v1 native question correlated to exact tool/message/session; ACP 1.19.4 lacks the provider continuation | Explicit once-only permission-shaped decision answers the native question; child transitions rejected | Both show plan reference/available excerpt and explicit leave/stay actions | [Leave-Plan contract](#opencode-leave-plan-confirmation); deterministic `opencode.rs` and client tests |
| `session/cancel` versus `$/cancel_request` ([v1][v1-cancel], [v2][v2-cancel]) | ✅ S | Session cancellation forwarded; v1 cancelled completion maps to standard v2 `cancelled`; RPC cancellation correlated per connection | Session cancellation withdraws decisions; RPC IDs remapped, never copied across hops | Both cancel work/questions and await authoritative idle; cancellation notice only in TUI | [Interaction broker][interactions]; `stop_reasons_and_native_failures_survive_the_gateway_and_replay` in [gateway tests][gateway-tests] |

### Configuration, discovery, usage, and compaction

| Feature / specification | Status | Ingestion: v1 / v2 | Daemon / client API | TUI / Web | Evidence / gap |
| --- | --- | --- | --- | --- | --- |
| `configOptions`, `config_option_update`, `session/set_config_option`: select/grouped choices ([v1][v1-config], [v2][v2-config]) | 🔀 A | V1 IDs/groups/setters translated; native v2 delegated | Advertised options validated; idle owner-only setter; confirmed responses publish to subscribers without requiring an echo; newer observations win | TUI `Ctrl-P` selects models, `Ctrl-G` other settings; Web exposes separate selects | `native_modes_and_collaboration_settings_remain_independent_and_change_between_turns` in [gateway tests][gateway-tests]; [config adapter][config] |
| Boolean config options ([v1][v1-config], [v2][v2-config]) | 🟡 P | Typed translation/setters implemented; v1 boolean client marker not advertised | Advertised boolean values accepted; no blanket unknown-kind setter support | Both cache options but only offer select controls | [Config adapter][config]; [TUI picker][tui-ui]; [Web UI][web-ui] |
| Categories `mode`, `model`, `model_config`, `thought_level`; dependent config replacement ([v1][v1-config], [v2][v2-config]) | 🔀 A | Agent-defined category/choices retained; usable native configuration takes precedence as a complete surface | Full authoritative config set replaces snapshot and can remove choices; mode and collaboration mode remain independent | Select options exposed; unknown categories preserved, not collapsed into one mode control | [Config adapter][config]; [gateway tests][gateway-tests] |
| V1 modes, `session/set_mode`, `current_mode_update` ([session modes][v1-modes]) | 🔀 A | Without usable native config, translate discovered modes into `_hive_legacy_mode`; consume updates and call native setter | Exposed upstream through `session/set_config_option`; confirmed snapshots sequenced; restart rediscovers settings from original session | TUI `Ctrl-G` and Web settings expose fallback selector; idle pilot only | `legacy_modes_use_the_native_setter_and_reconcile_acknowledgements_and_observers` and `modes_are_rediscovered_from_the_original_session_after_daemon_restart` in [gateway tests][gateway-tests] |
| `available_commands_update`, initial available commands; slash-command text input ([v1][v1-commands], [v2][v2-commands]) | 🟡 P | V1 discovery dropped; native v2 update/new-response typed fields relayed, but newer draft discriminators are not retained | No retained current-command snapshot on broker resume; slash text still ordinary prompt content | Neither provides protocol command discovery/completion | [Workers/broker][daemon]; [reviewed draft differences](#reviewed-draft-differences); [client reducers][web-client] |
| `session_info_update`: title and last-activity timestamp ([v1 schema][v1-schema], [v2 schema][v2-schema]) | 🟡 P | V1 drops; native v2 relays | Does not become a standard-info snapshot; Hive catalog summaries are a separate broker surface | Neither consumes standard session-info updates | [Workers][daemon]; [session catalog](hive-api.md#session-catalog) |
| `usage_update`: context used/size, cumulative cost ([v1 schema][v1-schema], [v2 schema][v2-schema]) | 🔀 A | V1 consumed for accounting, not raw proxy; native v2 relayed and consumed | Hive aggregates through `_hive_usage`; context gauge is not billable-token usage | Both show available aggregates; missing data stays unknown | [Usage details](#token-usage-and-costs); [gateway tests][gateway-tests] |
| Experimental end-turn token usage ([v1 unstable][v1-unstable], [v2 unstable][v2-unstable]) | 🔀 A | Enabled; v1 prompt result / v2 idle update | Optional agent-reported turn totals, not individual model calls; durable Hive accounting | Both show Hive usage snapshots; no fabricated missing counts | [Usage details](#token-usage-and-costs); [gateway tests][gateway-tests]; [#31](https://github.com/noirbizarre/hive/issues/31) |
| `notice`: info/warning/error, title/description ([v1 schema][v1-schema], [v2 schema][v2-schema]) | 🟡 P | V1 not advertised and dropped; native v2 relay | Live/ring event only, not retained conversation state; v2 has no notice capability gate | Both ignore standard notices; Hive failure extensions render separately | [Worker match][daemon]; [client reducers][web-client] |
| `compaction_update`, `compaction_summary_chunk` ([v1 schema][v1-schema], [v2 schema][v2-schema]) | 🟡 P | V1 not advertised and dropped; native v2 typed relay | No compaction history projection; update marks limited, summary-chunk-only omission currently misses warning | Neither renders compaction state/summary | [Transcript][transcript]; [#62](https://github.com/noirbizarre/hive/issues/62) |

### MCP and additional experimental features

Every additional protocol gate below is disabled except end-turn usage (listed above).
The `unstable` umbrella is not enabled; it would not implicitly enable protocol v2 anyway.

| Feature / specification | Status | Ingestion: v1 / v2 | Daemon / client API | TUI / Web | Evidence / gap |
| --- | --- | --- | --- | --- | --- |
| Session MCP declarations: v1 stdio/HTTP/SSE; v2 stdio/HTTP ([v1][v1-setup], [v2][v2-setup]) | 🟡 P | V1 rejects nonempty declarations; native v2 forwards/persists on new | Saved declarations reused for recovery; incoming upstream resume declarations **not applied**; no MCP support advertised or tool-execution guarantee; v2 removed SSE | Neither composes MCP-server declarations | [Workers/broker][daemon]; `recovery_keeps_workspace_roots_and_mcp_connections_alongside_the_original_identity` in [persistence][persistence] |
| MCP-over-ACP, `mcp/message` request to client / notification to agent; `acp` server transport ([v1][v1-unstable], [v2][v2-unstable]) | ❌ U | `unstable_mcp_over_acp` disabled | No negotiated bridge or inner request/server routing | Neither exposes the bridge | [Workspace features][manifest] |
| Providers: `providers/list`, `providers/set`, `providers/disable` ([v1][v1-unstable], [v2][v2-unstable]) | ❌ U | `unstable_llm_providers` disabled | No ACP provider management; Hive's HTTP profile/provider catalog is a different surface | Neither exposes ACP provider credentials/configuration | [Workspace features][manifest]; [operational API](hive-api.md) |
| `session/fork` ([v1][v1-unstable], [v2][v2-unstable]) | ❌ U | `unstable_session_fork` disabled | Not advertised/forwarded; no fork ownership/inheritance orchestration | No fork UI | [Workspace features][manifest]; [gateway][daemon] |
| System prompt append/override ([v1][v1-unstable], [v2][v2-unstable]) | ❌ U | `unstable_session_system_prompt` disabled | No capability or lifecycle request orchestration; unrelated to ordinary prompt text | No system-prompt editor | [Workspace features][manifest] |
| Extended plans: v1 `plan_update`; both versions markdown/file plans and `plan_removed` ([v1][v1-unstable], [v2][v2-unstable]) | 🟡 P | `unstable_plan_operations` disabled; native v2 unknown payloads may survive generic decoding | No typed remove/file operation semantics; unfamiliar plan content is not a command to read a file | Generic v2 plan display only; no removal semantics | [Plan boundary](#native-acpv2-plans); [plan tests][plans] |
| `subagent_update`, child `capabilities.cancel`, `session_message`, `session_message_chunk` ([v1][v1-unstable], [v2][v2-unstable]) | ❌ U | `unstable_subagents` disabled | Unknown native v2 payload relay does not implement child association, control restrictions, or inter-session message identity | No native subagent/message workflow; Hive background tasks are separate extensions | [Workspace features][manifest]; [activity](#live-execution-activity) |

### Replay, errors, metadata, and transports

| Feature / specification | Status | Ingestion: v1 / v2 | Daemon / client API | TUI / Web | Evidence / gap |
| --- | --- | --- | --- | --- | --- |
| Resume `replayFrom: start`, retained history and current-state barrier ([v2 setup][v2-setup]) | 🟡 P | V1 load replays agent history; resume-only cannot; v2 requests retained history on restoration | Materialized message/tool transcript plus current plans, not full terminal/compaction history; fresh known-state barrier follows replay | Both use explicit history-gap decisions rather than silently resending prompts | [Transcript][transcript]; [replay details](#replay-cursors) |
| Replay cursors other than standard `start` ([v2 schema][v2-schema]) | 🟡 P | No generic downstream cursor translation | Hive supports `_hive_seq`, with a bounded ring; unsupported/expired/future cursors fail rather than inventing history | Both use Hive cursor/snapshot rules; not arbitrary agent cursors | [Replay details](#replay-cursors); [gateway tests][gateway-tests] |
| JSON-RPC errors/data and post-insertion failures ([v1][v1-errors], [v2][v2-errors]) | 🟡 P | Typed SDK RPC errors; v1 post-admission errors become `_hive_error` then idle | Controlled Hive diagnostics; native idle/error relayed and accompanied by a materialized failure; no arbitrary error-data preservation guarantee | Both render controlled failure companions including replay, without exposing arbitrary agent error payloads | `stop_reasons_and_native_failures_survive_the_gateway_and_replay` in [gateway tests][gateway-tests]; [error boundary](#protocol-boundary) |
| `_meta`: entity patches, request/response and notification envelope metadata ([v1][v1-ext], [v2][v2-ext]) | 🟡 P | Native typed update metadata retained; downstream envelope metadata discarded | Entity metadata patches by top-level key; new-response metadata replaced by Hive attachment metadata; resume reconstructed; activity filters provider metadata | No arbitrary metadata UI; native plan metadata preserved within typed boundary | [Workers][daemon]; [transcript][transcript]; [plans][plans] |
| Unknown/custom values, `_` extensions and unknown updates ([v1][v1-ext], [v2][v2-ext]) | 🟡 P | V1 closed typed update set; v2 schema preserves supported unknown branches, not unmodeled fields on every known type | Generic v2 update relay/ring replay; no blanket arbitrary-method forwarding or full-snapshot preservation | Unknown updates ignored; unfamiliar plans shown generically; never interpreted as authorization | [Workers][daemon]; [plan limitations](#native-acpv2-plans) |
| Absolute filesystem paths / 1-based tool locations ([v1][v1-tools], [v2][v2-tools]) | 🔀 A | Agent-provided typed paths/locations retained where present | No generic path remapping or location correction; cwd identity checked for resume | Tool details shown, not a full editor-navigation service | [Workers][daemon]; [TUI tool renderer][tui-transcript] |
| Newline-delimited stdio ([v1][v1-transports], [v2][v2-transports]) | ✅ S | SDK stdio client; explicit profile | Downstream process transport; not the client-facing daemon endpoint | Both use daemon network endpoints instead | [Compatibility tests][compat-tests]; [transports](transports.md) |
| HTTP/SSE and WebSocket ([transport documentation](transports.md)) | ✅ S | SDK endpoint clients for declared versions/transports | Both upstream adapter shapes tested; this is transport support, not proof of full remote-transport RFD conformance | Both connect through supported daemon transports | [Gateway tests][gateway-tests]; [transport guide](transports.md) |
| JSON-RPC batch behavior ([v2 transports][v2-transports]) | ❔ Not assessed / unknown | SDK-managed parsing; no dedicated Hive batch-conformance evidence found | Do not infer lifecycle ordering/conformance from individual RPC tests | Built-ins send individual operations | [Gateway tests][gateway-tests]; [SDK provenance][sdk-pin] |
| Remote security / transport authentication ([transport guide](transports.md)) | 🟡 P | Configured agent endpoints may use TLS/headers | Hive gateway is trusted-user loopback only, not authenticated remote multi-user hosting | Both operate within that trust boundary | `a_remote_listener_is_rejected_without_authentication` in [gateway tests][gateway-tests]; [operations](operations.md) |

### Capability index

This index closes the inventory at capability-field granularity. Paths are relative
to v1 `agentCapabilities` / `clientCapabilities`, or v2 `capabilities`.
It maps each flag to a row above; it does not imply that copying a downstream
capability would make Hive support it. `_meta` on marker objects is metadata, not a flag.

| Side / baseline fields | Hive handling / status |
| --- | --- |
| V1 agent `loadSession`; `sessionCapabilities.list`, `.resume`, `.close`, `.delete` | **🔀 A** load/resume used for recovery; **🟡 P** list/close broker semantics; **❌ U** delete. No arbitrary downstream catalog aggregation. |
| V1 agent `promptCapabilities.image`, `.audio`, `.embeddedContext` | **🟡 P** inspected but v1 adapter still accepts text-only prompts. |
| V1 agent `mcpCapabilities.http`, `.sse`; implicit stdio MCP | **❌ U** declarations rejected on v1 path. |
| V1 agent `sessionCapabilities.additionalDirectories`; `auth.logout` | **❌ U** extra roots rejected; logout not orchestrated. |
| V1 client `fs.readTextFile`, `.writeTextFile`; `terminal`; `auth.terminal` | **❌ U** not advertised or implemented by the adapter. |
| V1 client `session.compaction`, `.notices`, `.configOptions.boolean` | **🟡 P** not advertised; compaction/notices dropped, boolean conversion exists without negotiated marker. |
| V1 client `elicitation.form`, `.url` | **🟡 P** handlers exist without advertising these modes in downstream v1 initialization. |
| V2 agent `session` | **🟡 P** advertised by Hive; baseline surface exists with resume limitations identified above. |
| V2 agent `session.prompt.image`, `.audio`, `.embeddedContext` | **🟡 P** typed native forwarding, not advertised upstream; no rich built-in composer. Text/resource links have no optional prompt marker. |
| V2 agent `session.mcp.stdio`, `.http`; `session.additionalDirectories` | **🟡 P** native new-session declarations/roots retained and reused for recovery, incoming upstream resume values ignored; support not advertised. |
| V2 agent `session.delete`; `auth` and `authMethods` | **❌ U** deletion/login/logout unavailable. `auth` alone is not the login capability; nonempty `authMethods` enables both auth methods. |
| V2 client `auth.terminal`; `elicitation.form`, `.url` | **❌ U** terminal auth; **✅ S** native broker advertises form/URL, checks each pilot's negotiated support separately. An empty elicitation object alone enables neither mode. |

V1 booleans default false; object markers in v2 use omitted/null = unsupported,
`{}` = supported. V2 `session: {}` includes new/list/resume/close/prompt/cancel/update;
there are no separate list/load/resume/close flags. V2 also has no client flags for
notices, compaction, boolean config, plans, or delegated filesystem/terminals.

| Experimental fields ([v1][v1-unstable], [v2][v2-unstable]) | Hive handling / status |
| --- | --- |
| Both agent `providers`; v1 `mcpCapabilities.acp` / v2 `session.mcp.acp` | **❌ U** providers and MCP-over-ACP disabled/unadvertised. |
| V1 `sessionCapabilities.fork` / v2 `session.fork` | **❌ U** fork disabled/unadvertised. |
| V1 `sessionCapabilities.systemPrompt.mode.override` / v2 `session.systemPrompt.mode.override` | **❌ U** system-prompt gate disabled; parent `systemPrompt` marker for append also not advertised. |
| V1 client `plan` | **❌ U** extended-plan reception marker not advertised; ordinary v1 `plan` adaptation is separate and supported. |
| V1 client `subagents`; both versions per-child `capabilities.cancel` | **❌ U** no child-session orchestration. V2 has no connection-wide client subagent marker. |

### Coverage boundaries

The pinned baseline registries contain **25 v1** and **16 v2** entries, including
notifications and `$/cancel_request`. Experimental registries contain **31 v1** and
**22 v2**, counting `mcp/message` separately on each side. All appear above.
There is no registered `session/set_model` or `session/compact` at this revision;
model selection uses config options, and compaction has notification variants.

The baseline update unions contain **14 v1** and **19 named v2** variants; the latter
also has an unknown-update branch. The tables cover every variant. Experimental
additions are v1 `plan_update` / `plan_removed`, and both versions' subagent and
session-message variants; v2 additionally adds `plan_removed` beyond its baseline.

The reviewed draft has the same baseline/experimental method counts, capability
fields, content-block kinds, and update discriminators: the comparison changes
negotiation semantics, command entry shapes, and experimental message fields,
not those registries. Command kinds now include `command` and custom/future
discriminators; the released typed boundary still has the older shape described
in [Reviewed draft differences](#reviewed-draft-differences).

Nested features are included under their owning surface rather than treated as
separate methods. Specifically, the audited known values are:

- Tool kinds: `read`, `edit`, `delete`, `move`, `search`, `execute`, `think`,
  `fetch`, `switch_mode`, `other`. Statuses: `pending`, `in_progress`, `completed`,
  `failed`, plus v2 `cancelled`.
- Plan priorities: `high`, `medium`, `low`. Entry statuses: `pending`,
  `in_progress`, `completed`, plus v2 `cancelled`.
- Permission option kinds: `allow_once`, `allow_always`, `reject_once`,
  `reject_always`; outcomes: selected exact `optionId` or `cancelled`.
- Stop reasons: `end_turn`, `max_tokens`, `max_turn_requests`, `refusal`,
  `cancelled`, plus v2 `error`. V2 may carry fatal-error details with idle.
- Notices: `info`, `warning`, `error`. Compaction statuses: `in_progress`,
  `completed`, `failed`, `cancelled`, with identified streamed summaries.
- Content blocks: `text`, `image`, `audio`, `resource_link`, `resource`;
  embedded resources contain text or base64 blobs. Annotations include
  audience (`assistant`/`user`), priority, and last-modified time.
- Elicitation: string/enum, boolean, number/integer, string-array multiselect,
  required/default and validation constraints; results `accept`, `decline`, `cancel`.

Future/custom values are subject to the version-specific extensibility and typed
deserialization limits above, not automatically understood by Hive's clients.

## Hive extensions inventory

These are **Hive contracts**, not standard ACP capabilities. External clients must
implement the relevant Hive metadata contract to mutate/attach safely. They cannot
infer it merely from negotiating protocol v2.

| Surface | Status | Boundary and built-in client exposure | Reference |
| --- | --- | --- | --- |
| `_meta.hive.agent`, default profile, OpenCode `adopt` | ✅ S | Client-to-Hive selection/adoption consumed by broker; ACP adoption is not generic native-session discovery | [Protocol boundary](#protocol-boundary), [OpenCode adapter](#opencode-server-adapter); ACP Pi adoption [#6](https://github.com/noirbizarre/hive/issues/6) |
| Generation/token, pilot/observer/relinquish, `_hive_control` | ✅ S | Broker ownership across transports; both built-ins support handoff and observer state | [Control and replay](#control-and-replay), [ownership](#mutation-ownership-and-idempotency) |
| Prompt key/fingerprint and `recoveryAction` | ✅ S | Durable deduplication/recovery fence; no automatic redispatch; future hardening is not complete certainty about provider execution | [Ownership](#mutation-ownership-and-idempotency); [#56](https://github.com/noirbizarre/hive/issues/56) |
| `_hive_seq`, `seq`/`nextSeq`, snapshot markers/watermarks | ✅ S | Bounded incremental ring plus materialized start snapshot; both clients handle gap decisions | [Replay cursors](#replay-cursors) |
| `_hive_v1_tool_call`, `_hive_v1_tool_update`, `_hive_v1_plan`, `_v1_stop` | 🟡 P | Explicit compatibility wrappers for v1 tools/plans; `_v1_stop` is only a future non-equivalent stop-reason fallback, not the mapping for currently known reasons | [Tool/plan rows](#tools-terminals-plans-and-interactions), [activity](#live-execution-activity) |
| `_hive_activity`, `_hive_task`, `activitySnapshot` | 🟡 P | Convenience tool/plan/foreground projection shown by both UIs; bounded tool chunk suffix resets on content replacement; background tasks require extension IDs, not inferred subagents | [Live execution activity](#live-execution-activity) |
| `planSnapshot`, config snapshots/sequence, transcript status | ✅ S | Attachment state independent of bounded replay; both clients fence old replay; agent retention still determines restart completeness | [Native plans](#native-acpv2-plans), [replay](#replay-cursors), [transcript status](hive-api.md#transcript-ownership-and-status) |
| `_hive_usage`, usage snapshot/cost estimates | 🔀 A | Hive accounting depends on agent reports and optional price identity; both UIs show available totals | [Token usage and costs](#token-usage-and-costs) |
| `_hive_error`, `_hive_opencode_error` | ✅ S | Controlled failure diagnostics rendered by both clients, not standard `notice` support | [Protocol boundary](#protocol-boundary), [OpenCode diagnostics](#opencode-diagnostics) |
| HTTP catalog, provider/native-session discovery, invalidation feed | ✅ S | Operational API alongside ACP; no implication of standard ACP providers/auth or arbitrary native ACP session adoption | [Operational API](hive-api.md), [OpenCode adapter](#opencode-server-adapter) |

## Maintaining this audit

1. On ACP dependency upgrades, record resolved crate versions, enabled features,
   published source commits, audit date, and Hive evidence revision. Consult
   specification docs **at that source revision**, not only the live website.
2. Compare both versions' method registries, capability definitions, content/update
   unions, and baseline/experimental schemas. Add new and removed surfaces explicitly;
   do not rebuild the list from Hive's handlers alone.
3. Trace each changed row through ingestion, daemon state/materialization, upstream
   advertisement/relay, TUI, and Web UI. Verify null/omission and unknown-value behavior
   through the pinned typed boundary. Cite named tests or implementation evidence.
4. Recheck issue state. A closed implementation issue is evidence only for its tested
   scope. A gap issue does not promise optional support. Remove unknown assessments
   when sufficient evidence exists, and record unassessed additions explicitly.
5. Keep real-agent claims version/date/transport-specific. Retain probe summaries as
   described below; deterministic mocks do not upgrade product evidence. Cross-agent
   coverage remains [#45](https://github.com/noirbizarre/hive/issues/45), and programmable
   Bee test infrastructure remains [#57](https://github.com/noirbizarre/hive/issues/57).
6. Check navigation, preserved anchors, Markdown/spelling and the site build:
   `mise run lint:md`, `mise run spell`, `mise run docs:build`. Run relevant existing
   gateway/client tests when refreshing behavioral claims; real-agent probes are opt-in.

## Protocol boundary

Hive clients always initialize with ACP protocol version 2. The current SDK
`agent-client-protocol =3.5.0` needs its `unstable_protocol_v2` feature.
`agent-client-protocol-http =3.5.0` provides both server transport shapes.
Their crate versions do not denote the ACP wire version. The SDK pins schema
`=1.12.0`. The Cargo lockfile records the resolved dependency set.

Implemented v2 operations are initialize, session/new, list, resume, close,
prompt, cancel, session/set_config_option, and session/update. A prompt response acknowledges acceptance.
Observe `running` then `idle` for work completion.

Only text prompts are adapted to v1. V1 message chunks are forwarded with a
synthetic v2 message ID. Its completed prompt response generates a v2 idle
update. V1 tool calls are preserved as `_hive_v1_tool_call`/`_hive_v1_tool_update`
events. Permissions and supported elicitation are forwarded to the pilot.

| Stop/failure path | Hive behavior | Deterministic verification |
| --- | --- | --- |
| V1 successful prompt result | Preserves `end_turn`, `max_tokens`, `max_turn_requests`, `refusal`, and `cancelled` as equivalent v2 idle reasons; future non-equivalent SDK reasons fall back to `_v1_stop` | Daemon mapping unit test and gateway stop-reason regression |
| V1 forwarded cancellation | Only a downstream `cancelled` result confirms cancellation; a different result or RPC failure keeps its own outcome | Gateway in-flight cancellation regression |
| Native v2 idle/error | Forwards native state and emits one controlled `_hive_error` companion for both clients | Gateway, daemon projection, and client reducer regressions |
| Repeated idle/error snapshots | Retains last-reported failure context once per foreground interval/idle episode, without adding completions | Daemon projection and client replay regressions |

V2 permissions and
session-scoped form/URL elicitation are forwarded to the pilot with a 120-second
total decision deadline. Request-scoped elicitation follows a uniquely mapped,
live parent RPC to its original requester. Unknown modes and unmapped parent
contexts are cancelled.

MCP bridge and forks are **not** advertised. Native v2 compaction updates are relayed,
but neither built-in client renders them and materialized history omits them.
V1 compaction reception is not advertised. Non-text v1 prompts return an error.

Agents can expose session configuration through `configOptions` on new/resume
responses and `config_option_update` notifications. Hive retains the current
snapshot independently of replay and forwards `session/set_config_option` for
idle sessions owned by the caller. ACPv1 option/group identities and setter
values are translated to ACPv2. Response `_meta.hive.configSeq` marks the
snapshot boundary so older replay cannot undo current selections. Support is
covered by v1/v2 mocks; actual choices depend on the downstream agent.
The native OpenCode server adapter does not expose session configuration.

### Session modes and agent settings

Hive prefers usable native `configOptions` as a complete surface, preserving
names, values, grouping, ordering, descriptions, and unknown categories.
Selections use the advertised config ID through `session/set_config_option`.
When no usable native configuration is available, ACPv1 `modes` become a
mode-category configuration selector whose setter calls native
`session/set_mode`. Legacy `current_mode_update` notifications refresh that
selector. Native configuration takes precedence to prevent duplicate controls.

All settings, including model, remain changeable after startup by the pilot of
an idle session. TUI `Ctrl-P` selects models; `Ctrl-G` selects other settings.
Both clients display confirmed current values. An empty legacy acknowledgement
confirms the requested selection only if no newer agent observation superseded
it. Failed changes do not invent a successful selection. Attachments use the
current sequenced snapshot; restart rediscovers settings from the saved agent
session instead of applying a Hive default. Plans and activity never determine
the selected mode.

Source-inspected compatibility on 2026-10-26 (runtime verification is recorded
separately below):

| Adapter/source version | Advertised settings | Meaning |
| --- | --- | --- |
| [OpenCode 1.19.4](https://github.com/anomalyco/opencode/blob/v1.19.4/packages/opencode/src/acp/config-option.ts) | `mode`, category `mode` | Primary agent, including Build/Plan and custom agents |
| [Claude agent ACP 0.96.0](https://github.com/agentclientprotocol/claude-agent-acp/blob/f6958db1ee8d3e9ead6767a3a294f98b9a21adb9/src/session-mode.ts) | `mode`, category `mode` | Permission mode, including Plan; available choices depend on runtime settings |
| [Codex ACP 2.2.2](https://github.com/agentclientprotocol/codex-acp/blob/51005a6e11fd2cb7caa2d27fcfdd5ad0fbf41027/src/CollaborationModeConfig.ts) | `mode` and `collaboration_mode` | Independent approval/sandbox preset and Default/Plan collaboration selector |
| [Pi ACP 0.0.34](https://github.com/svkozak/pi-acp/blob/04d0a15d6bb42bb65cce5c9caedcdcb2cdcb6905/src/acp/agent.ts) | `model` and `thought_level` | Provider/model and reasoning effort; legacy modes duplicate thinking level |

Hive forwards reported configuration changes to every attached subscriber,
including changes initiated by another Hive client. Visibility of changes made
outside Hive depends on the adapter emitting updates: OpenCode 1.19.4 and
Codex's direct setters return configuration snapshots without notification
echoes; later OpenCode source and agent-driven Codex transitions report updates.
Hive cannot infer an unreported external change. Configuration setter responses
still update all Hive clients. These source findings do not upgrade the retained
live-agent matrix below.

Runtime settings evidence, 2026-10-26: the ignored
`real_agent_settings_can_change_and_restore_without_inference` probe passed
against installed OpenCode **1.19.4**, `opencode acp`, downstream ACPv1, upstream
WebSocket. It discovered Session Mode, changed and restored an advertised value,
and confirmed the restored selection on attachment without inference. This
verifies that settings path only; it does not establish prompt behavior,
arbitrary external-change notifications, or other adapters' live support.

Post-admission ACPv1 prompt failures produce a sequenced `_hive_error` update
before idle. Its version, source, operation, stable diagnostic code, message
and help contain controlled diagnostics rather than arbitrary provider data.
Both clients render these failures, including on replay.

Native v2 `state_update` with `state: "idle"` and `stopReason: "error"`
also generates a sequenced `_hive_error`, after the native state update, with
`source: "acp-v2"` and `operation: "foreground"`. Known expired-key messages
produce the established actionable diagnostic. Missing, malformed, or unknown
error details produce `hive::agent::foreground_failed` with controlled message
and help. Arbitrary downstream messages, error data, and metadata are never
copied into these displayed/retained failure entries. Native protocol updates
are still forwarded unchanged; this is a presentation/history guarantee, not
redaction of the raw ACP stream. Existing `_hive_opencode_error` rendering is
preserved. Idle always ends foreground work, including when the stop reason is
omitted, malformed, or unknown; an unreported outcome does not imply success.

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

<a id="installed-agent-capability-matrix-2026-10-11"></a>

## Known agents

These rows distinguish **known ACP entrypoints**, **inspected local binaries**, and
**verified behavior**. Local CLI availability was last checked on **2026-10-25**;
each row retains its recorded product version rather than implying every entry
has been tested on that date. The evidence labels mean:

- **✅ Guaranteed:** demonstrated for this installation by a completed probe.
- **🟡 Best-effort:** observed but dependent on timing, model, or configuration.
- **❌ Unsupported:** missing entrypoint or Hive operation.
- **❔ Unverified:** no successful live probe.

The v1/v2 declaration on a profile does **not** prove
the downstream agent accepts that wire version. Re-run the opt-in probes after
upgrading an agent or adapter and record results with its product version and transport.

| Product/version | ACP entrypoint/transport | ACP wire | Fresh `session/new` | Native-created load/resume over ACP | Hive live-worker resume |
| --- | --- | --- | --- | --- | --- |
| Pi 1.3.0 via external `pi-acp` 0.0.34 (see [Pi](#pi)) | `pi-acp` or `npx -y pi-acp`, stdio. Pi itself has no ACP entrypoint | v1 negotiated (2026-10-23); a v2 `initialize` is answered with v1 | ❔ Unverified (only `initialize` is verified) | Adapter advertises `loadSession` via its own session map; ACP attachment unverified; Hive adoption is [#6](https://github.com/noirbizarre/hive/issues/6) | ❔ Unverified with Pi (guaranteed with mock) |
| OpenCode 1.18.35 (local CLI inspection) | `opencode acp`, stdio documented and command present | v1 documented; v2 unverified | ❔ Unverified | Native `session list` and `run --session` available; ACP load unverified | ❔ Unverified with OpenCode (guaranteed with mock) |
| Claude Code 2.1.310 (local CLI available, 2026-10-25); ACP adapter not found on `PATH` | External [`claude-agent-acp`][claude-acp] / `@agentclientprotocol/claude-agent-acp`, stdio, backed by the Claude Agent SDK; [setup](agents/claude.md) | Upstream documents v1 and draft v2; local adapter/version negotiation ❔ unverified | ❔ Unverified | Native `claude --resume` exists; ACP load/resume and Hive adoption ❔ unverified | ❔ Unverified |
| Codex CLI 0.164.0 (local CLI available, 2026-10-25); ACP adapter not found on `PATH` | External [`codex-acp`][codex-acp] / `@agentclientprotocol/codex-acp`, stdio, backed by Codex App Server; [setup](agents/codex.md) | ❔ Unverified; record the selected adapter version and `initialize` result | ❔ Unverified | Native `codex resume` exists; ACP load/resume and Hive adoption ❔ unverified | ❔ Unverified |

| Product/version | Text prompt | Streaming events/idle | Tool calls/results | Permissions | User-input requests | Cancellation | Active client disconnect |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Pi 1.3.0 via `pi-acp` 0.0.34 | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified |
| OpenCode 1.18.35 | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified |
| Claude Code 2.1.310; adapter not locally probed | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified |
| Codex CLI 0.164.0; adapter not locally probed | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified | ❔ Unverified |

Claude Code and Codex availability was checked with `claude --version`, `claude --help`,
`codex --version`, and `codex --help`. Neither inspected CLI help lists a native ACP
entrypoint; their stream-JSON and App Server interfaces are not themselves ACP.
Use the separately distributed adapters linked above. Their upstream feature lists
are adapter claims, not verified Hive end-to-end results. No inference prompt was sent.

The Claude adapter documents automatic v1/draft-v2 negotiation, but its current
draft surface can differ from Hive's pinned SDK. The Codex adapter's npm package
includes a compatible Codex dependency; its runtime need not be the local 0.164.0 CLI.
Pin and record the **adapter and runtime versions** when probing either integration.
The current Codex adapter is maintained under `agentclientprotocol`; the older
`@zed-industries/codex-acp` project directs new installations there.

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

For v1/v2 agents over **ACP**, Hive's upstream ACPv2 resume and conversation
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
not by the Known agents rows marked unverified. A tool-free probe does not establish
support for those optional features. See
[the reproducible probes](development.md#real-agent-compatibility-probes).

## Control and replay

The [pinned draft v2 specification][v2-setup] defines this audit's contract;
newer drafts may change. See the [SDK v2 guide][sdk-v2] and [HTTP guide][sdk-http]
at the SDK's source revision.
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
native v2 agents, retaining each upstream client's negotiated support separately.
The v1 adapter has interaction handlers but does not advertise form/URL modes
in its downstream initialization.

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
event with `generation`, pilot presence and `recoveryRequired`, but no token. `session/list` includes
profile, running, generation, pilot presence and observer count in each entry's
`_meta.hive`, along with the shared `title`, `lifecycle`, `activity`,
`lastActivityAt` and `controlOwner` summary fields. The owner is an ephemeral
connection ID, **never** the token. An absent title/timestamp/owner is `null`.
`activity: "unknown"` means no trustworthy foreground status is available.
The HTTP catalog and its SSE invalidation feed are documented in
[Hive's operational API](hive-api.md#session-catalog). ACP `session/list` and
HTTP snapshots derive their fields from the same broker.

A close is explicit and admits running, blocked, or configuring work. It withdraws
pending permissions and elicitation routes, fences new work and takeover, and
waits for final updates and terminal idle before reporting successful cancellation.
Native ACPv2 close supplies the downstream ordering barrier; the v1 adapter sends
cancel and publishes idle from the actual turn completion before detaching.
A normal completion that wins the cancellation race keeps its real stop reason;
closing an already idle session adds no synthetic idle. Downstream failure is a
close error, not proof of cancellation; recoverable close errors retain the worker
and release the closing fence so the owner can inspect state and retry.

Pending prompt acknowledgements are insertion-aware: an inserted message retains
its successful `messageId`, while insertion prevented by close returns
request-cancelled (`-32800`). All same-key waiters share that result without a
second dispatch. Downstream prompt responses can follow the downstream close
response; Hive retains the connection to collect them before releasing it.
Cancelled journal outcomes survive restart like successful acknowledgements.
These paths are covered over both upstream transports by
`active_close_orders_final_updates_and_preserves_insertion_outcomes_on_both_transports`,
`close_cancels_native_and_legacy_configuration_requests_on_both_transports`, and the
permission/form/URL close regressions in [gateway tests][gateway-tests].

Client detach does not close the session or cancel the agent. A second distinct
prompt or configuration change is rejected during running or `requires_action`
foreground work (not merely until its prompt response), including blocked-first
and restored blocked sessions. Compatibility `running` booleans include both
active states, while catalog `activity` preserves their distinction. Unknown
foreground remains unknown; an uncertain prompt additionally fences new work
until recovery is explicitly acknowledged.

Prompts additionally require `_meta.hive.key`, a nonempty client-generated
idempotency key scoped to the Hive session. Retrying the *same key and content*
after an uncertain acknowledgement returns or waits for the original result,
including after takeover. The same key with changed content fails.

A rejected prompt that was never dispatched retains its error under that key.
Use a new key for a newly intended prompt. Do not blindly send a new key after
losing an acknowledgement. Reconnect, observe the original session, and explicitly
retrieve its existing receipt using the original key/content. This never queues
another downstream execution, including after daemon restart.

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
acknowledgement. Proven pre-dispatch rejections and uncertain delivery remain
distinct; transport errors do not establish execution failure. Controlled
terminal evidence is separate from acceptance. An idle observed before the
receipt and a receipt observed before idle converge on the same durable state
when both are reliably correlated. Replayed history and usage cannot supply a
missing receipt or terminal result.
Normal live acknowledgements and in-memory retries may still include protocol
metadata. Schema 3 preserves schema-2 fingerprints/receipts, adds evidence and
durable recovery acknowledgement, and conservatively fences legacy ambiguity;
see [storage and backups](operations.md#storage-and-backups).

### Prompt recovery diagnostics

JSON-RPC errors use standard ACP error categories and a controlled
`error.data.hive` object with `version: 1`, `code`, `message`, `help`,
`recoveryRequired` and `retry: "receipt_only"`. Match `code`, not message text.

| Stable code | Meaning / next action |
| --- | --- |
| `hive::prompt::outcome_uncertain` | Original outcome is unknown; never redispatch this key. Inspect the session and external effects; acknowledge if required. |
| `hive::prompt::not_dispatched` | This attempt provably did not publish. Keep its key reserved; use a new key for new intent. |
| `hive::prompt::key_conflict` | Original key has different content; retrieve with original content or submit new intent under a new key. |
| `hive::session::recovery_required` | Observe first; explicitly acknowledge via `session/resume` before taking control for new work. |
| `hive::prompt::journal_failed` | Evidence could not be persisted/read; new work is blocked. Repair storage and inspect the original session. |

After recovery acknowledgement, looking up an uncertain key still returns
`outcome_uncertain`, with `recoveryRequired: false`. This does not raise the old
incident again. Acknowledgement neither changes the old outcome nor skips replay,
proves idle, or certifies transcript completeness. Clients retain original pending
identity across transport loss and reject stale asynchronous receipt results.

## Replay cursors

Live `session/update` events carry `_meta.hive.seq`, an increasing per-session
integer, unchanged on incremental replay. On `session/resume`, omit `replayFrom` to attach
without history. `{ "type": "start" }` requests a materialized transcript snapshot.
`{ "type": "_hive_seq", "seq": N }` requests updates **at or after** N,
inclusive. Send `lastProcessedSeq + 1` to avoid duplicates. `nextSeq` in the
resume response identifies the first event not yet produced at the atomic
takeover boundary.

When foreground state is known, every successful observer or pilot resume emits
a fresh standard `state_update` after replay and before the response, over both
WebSocket and HTTP. The barrier has a new ordinary sequence, is broadcast to all
attached subscribers and occupies an entry in the replay ring. It does not alter
the transcript snapshot boundary. Idle barriers omit previous stop reasons and
token usage: synchronization must not repeat completions, cancellations, failures
or accounting. Attachment metadata describes the same current foreground state.

**Unknown-state exception:** Attachment still succeeds when no authoritative
foreground state is available, such as a new session before any state report or
a restoration that omitted current state. Hive keeps foreground unknown and
emits no standard state barrier rather than inventing idle. Such a successful
response is not proof of idle and does not fulfill the standard state-barrier
requirement. A later authoritative state update resolves this uncertainty.

For incremental replay, the most recent 256 updates (including chunks and state
transitions) are available. An expired or future cursor fails with the current
available range and leaves the old controller in place. Live updates are
ordered after replay. A partially delivered replay must be retried using the
last actually processed sequence.

Full transcript snapshots are independent of this bounded ring. They contain
full message/tool upserts and controlled failure entries in first-appearance
order. Snapshot notifications carry
`_meta.hive.snapshot: true` and share a boundary sequence; they are not contiguous
incremental events. The resume response's `_meta.hive.snapshotThroughSeq` gives
that boundary. Stage marked entries until the response succeeds, replace the
conversation view, set the incremental cursor to this boundary, then process
queued ordinary events above it. Do not append ring chunks to the snapshot or
advance past takeover events emitted after snapshot capture. Current activity,
usage, configuration and running state come from the attachment response.

Failure entries are separate from live state synchronization: snapshots do not
replay historical running/idle transitions. Rebuilding failure history cannot
stop current foreground work or repeat usage accounting. Incremental replay
retains each controlled failure's original sequence for client deduplication.
Repeated idle snapshots do not append extra failures; a new `running` or
`requires_action` interval permits a new failure even with identical details.
A first-seen idle/error can supply last-reported failure context, without
establishing a newly observed completion. The TUI likewise does not turn repeated
idle/cancelled snapshots into additional cancellation notices.

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
Failure entries survive incremental-ring eviction while Hive remains alive.
After restart, they can only be reconstructed from downstream replay evidence;
a final idle/error snapshot is last-reported context, not proof of complete
failure history. Historical failure import remains silent and does not
acknowledge interrupted work.

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

Turn completion requires an observed foreground boundary: after dispatch, the
agent reports `running` or `requires_action`, followed by `idle`. An initial or
repeated idle arriving before that active state is a session snapshot, not the
pending prompt's completion; Hive ignores it without releasing prompt admission
or requiring recovery. This follows the pinned [v2 lifecycle][v2-prompt], which
allows initial state after `session/new` and requires active-state reporting.
Neither the prompt response nor content/background updates substitute for it.

A terminal idle may omit both stop reason and usage: Hive still records the
turn, with null tokens when unreported. A native v2 agent that emits only idle
without first reporting an active state violates that lifecycle; Hive cannot
attribute its idle to the prompt, so admission remains reserved and no turn is
recorded. Pi through `pi-acp` and OpenCode ACP use Hive's v1 adapter, which
synthesizes the active boundary; the OpenCode server adapter supports bare idle
after observed running. These adapter/source checks do not expand the retained
real-agent runtime evidence described in the agent sections.

Regressions include `a_pre_work_idle_preserves_admission_and_the_real_turn_usage`
and `a_bare_idle_after_foreground_work_records_one_turn_without_tokens` in
`crates/hive-daemon/src/lib.rs`, and
`an_idle_after_dispatch_preserves_usage_and_does_not_fence_the_next_prompt` in
[gateway tests][gateway-tests]. The gateway fixture emits its early idle only
after receiving the downstream prompt, so the ordering is deterministic.

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

## Native ACPv2 plans

Hive's pinned SDK (3.5.0, schema 1.12.0, `unstable_protocol_v2`) accepts native
`session/update` notifications with this shape:

```json
{
  "sessionId": "hive-example",
  "update": {
    "sessionUpdate": "plan_update",
    "plan": {
      "type": "items",
      "planId": "execution",
      "entries": [
        {"content": "Inspect implementation", "priority": "high", "status": "completed"},
        {"content": "Verify changes", "priority": "medium", "status": "in_progress"}
      ]
    }
  },
  "_meta": {"hive": {"seq": 12}}
}
```

Each itemized update replaces the **entire ordered entries array** for its
session-scoped `planId`. Entries are not patches or individually identified
tasks: this schema has no standard entry-ID field. Empty entries clear the list
without removing the plan. Idle does not clear plans. Priorities and statuses
(including `pending`, `in_progress`, `completed`, `cancelled`, and unknown string
values) retain their native meaning. Hive neither sorts entries nor infers task
identities from their descriptions.

The gateway broadcasts the complete typed native update to every attached client
authorized to observe that session. Native updates retain their update, plan,
and entry `_meta` fields. Unknown plan content types retain their discriminator,
ID, and extension fields; clients may ignore them or show generic escaped content.
Hive does not enable unstable plan operations or interpret unknown variants as
instructions to read files, fetch URLs, or remove plans.

Preservation is at the pinned SDK's **typed deserialization boundary**, not a
byte-for-byte JSON proxy guarantee. In particular, invalid itemized entries are
skipped, malformed entries collections can normalize to empty, and unmodeled
fields on known types can be discarded by that schema. Missing/non-string IDs
and invalid known content schemas fail deserialization rather than becoming
invented plans. Malformed notifications do not establish support observation.

### Current plan snapshot and support

New/resume responses include `_meta.hive.planSnapshot`:

```json
{
  "version": 1,
  "seq": 12,
  "support": "supported",
  "observation": "observed",
  "plans": {
    "execution": {
      "plan": {
        "type": "items",
        "planId": "execution",
        "entries": []
      }
    }
  }
}
```

Each map value is the complete latest `PlanUpdate` object (`plan` and optional
`_meta`, without the `sessionUpdate` discriminator). This is the canonical volatile
native plan projection. The filtered `_hive_activity` plan view remains a
compatibility convenience, separate from `_hive_task` background activity; do not
render both as independent plans.

`support: supported` describes Hive's native plan transport and itemized client
rendering contract. ACP exposes no dedicated agent plan capability flag, so this
does not promise the agent will emit a plan. `observation` is `not_observed` until
a valid native plan arrives, then `observed`, including empty plans and unknown
content types. An older daemon without this metadata has unknown support. A
client's unsupported rich rendering of a particular content type is distinct
from unknown agent emission; both built-in clients display unfamiliar content
generically. Silence never means unsupported. V1 plans continue through the
separate `v1-plan` activity adaptation (`plan.source: acp-v1`) and do not establish
native v2 observation. When a canonical snapshot is present, clients must not
resurrect native plans from an older activity projection; only explicitly marked
v1 adaptations remain a fallback. Receiving an individual native update does not
establish snapshot completeness: when attached to an older daemon without a
canonical snapshot, retain other activity-derived plans until their native
updates arrive. V1 fallback content does not change the native observation label.

### Attachment and replay

- Snapshot `seq` is the session's atomic attachment boundary (last included
  session sequence), not a separate plan counter. Buffer notifications arriving
  before the response; reconcile the snapshot before reducing buffered events.
  Apply newer native plans as complete replacements and ignore plans at or below
  the snapshot watermark. Continue consuming unrelated events with the ordinary
  session cursor; a plan watermark must not suppress other activity.
- Incremental `_hive_seq` replay retains original native plan updates, ordered
  with other session events, within the 256-event ring.
- `replayFrom: start` emits each current native plan alongside the materialized
  transcript, using `_meta.hive.snapshot: true` and the shared snapshot boundary.
  Multiple snapshot notifications can share that sequence: use snapshot upserts,
  not ordinary duplicate-event suppression. It restores current plans, not every
  historical plan revision.
- Live-only attachment includes the current plan snapshot even after original
  plan events expire. On an expired cursor, use the existing explicit history-gap
  decision, restore the snapshot, and continue live from `nextSeq`.
- A daemon restart loses the volatile projection. During downstream restoration,
  agent-replayed plans rebuild it without republishing historical activity or
  repeating accounting. If the agent does not retain/replay plans, the new
  projection is empty and `not_observed`; Hive does not manufacture a plan,
  create a replacement session, or prompt the agent to reconstruct one.

## Live execution activity

Native ACPv2 tool, plan, and state updates remain on the shared session stream.
Immediately after each supported native update Hive publishes a sequenced
`session/update` with `sessionUpdate: "_hive_activity"`, `version: 1`, and fields
selected by `kind`:

| Kind | Fields | Meaning |
| --- | --- | --- |
| `tool` | `toolCallId`, `tool`, `source` | Current full tool-call state after applying an ACP patch or content chunk. `status` uses ACP's `pending`, `in_progress`, `completed`, `failed`, `cancelled` values. Raw input/output and result/error content are retained when sent. |
| `plan` | `planId`, `plan`, `source` | Full replacement of plan content, including its type and ordered entries/statuses when itemized. Unknown content remains generic. A v1 plan uses the session-scoped ID `v1-plan`. |
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
- In the canonical transcript, content chunks append until a full content update
  replaces them. In normalized activity, `content` is the latest authoritative
  replacement and `chunks` is a separate bounded suffix of subsequent chunk
  envelopes. Any explicit `content` update, including `null` or `[]`, removes
  both `chunks` and `chunksTruncated`. Null removes the `content` field; an empty
  array retains an empty `content` array. Later chunks start a fresh suffix;
  omitted `content` preserves the current content, suffix, and truncation flag.
- Normalized tool events are full entity upserts: both clients replace the
  previous tool object, so removed fields cannot survive through a client merge.
- Top-level provider `_meta` is excluded from these convenience projections;
  native plans and `planSnapshot` preserve typed metadata.

IDs are scoped to the Hive session.
Turn IDs, parent task IDs, and other relationships are present only when the
downstream event provides them. A tool or task update after idle never reopens
the foreground turn or closes the session.

`_meta.hive.activitySnapshot` in new/resume responses contains `{version:1,
seq,session,foreground,tools,plans,tasks,support}` at the atomic replay/live attachment
boundary. `seq` is the last produced session-update sequence at that boundary.
`nextSeq` remains the
first not-yet-produced *session update*.

The support values for tools, plans,
and tasks are `observed`, `unknown`, or `unsupported`. V1 task identities are
unsupported by Hive's stable v1 adapter. Otherwise an absent plan update cannot
justify an invented plan or an unsupported claim. Unrecognized provider statuses
are preserved.

Finished tools are retained up to 128 per session. Streaming chunk snapshots
retain the latest 64 chunks after the most recent content replacement and
flag `chunksTruncated` when earlier chunks in that suffix have been evicted.
A replacement resets this flag along with the chunk buffer. Raw event history
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
overwritten. A daemon restart loses this in-memory snapshot. Agent history can
restore conversation text and replayed plans; tools/tasks are not reconstructed
as current activity from historical events. Foreground activity remains unknown
unless the agent supplies current state, rather than assuming interrupted work was idle.
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
| 1.19.3 | v1 routes | ✅ Guaranteed | ✅ Guaranteed | 🟡 Best-effort (model dependent) | ✅ Guaranteed | ✅ Guaranteed |
| 1.19.3 | `/api/*` v2 preview | 🧪 Fake only | 🧪 Fake only | 🧪 Fake only | 🧪 Fake only | ❔ Unverified |
| below 1.19.0 | any | ❌ Unsupported: `hive::opencode::unsupported_version` | ❌ Unsupported | ❌ Unsupported | ❌ Unsupported | ❌ Unsupported |

| OpenCode | Cancel | Permissions | Questions | Stream drop and reconnect | Restart recovery | External client detected |
| --- | --- | --- | --- | --- | --- | --- |
| 1.19.3 v1 | 🧪 Fake only | 🧪 Fake only | 🧪 Fake only | 🧪 Fake only | 🧪 Fake only | 🧪 Fake only |
| 1.19.3 v2 preview | 🧪 Fake only | 🧪 Fake only (polled) | ❌ Unsupported | 🧪 Fake only | 🧪 Fake only | 🧪 Fake only |

| Entry point | `plan_exit` confirmation | Evidence |
| --- | --- | --- |
| Server v1, OpenCode 1.x | 🧪 Deterministic tests; requires correlated native question | 1.19.4 source contract; fake gateway and both client tests |
| `opencode acp`, 1.19.4 | ❌ Provider does not expose built-in `plan_exit` or forward native questions | Source inspection; no synthetic continuation in Hive |
| Server `/api/*` v2 preview | ❌ Questions unsupported | No affirmative fallback |

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
  to Hive again, except for cancelled plan-exit confirmations described below.

### OpenCode leave-Plan confirmation

Hive recognizes server-v1 `plan_exit` at its blocking native question, using
`tool.messageID` and `tool.callID` to retrieve and validate the exact assistant
message/tool part. Tool notifications alone never authorize execution. Missing
or mismatched linkage cannot authorize a specialized transition; unresolved
tool-linked questions wait for reconciliation rather than falling back to a
generic approval form. Other verified tools retain normal question handling.

Both clients show **OpenCode: leave Plan mode?**, the provider request/plan
reference and a bounded visible assistant excerpt where supplied. Missing or
truncated context is identified. Hive does not fetch an arbitrary referenced
plan file. **Leave Plan and start implementing** sends exactly `[["Yes"]]`
through the native question-reply endpoint. **Stay in Plan / refine the plan**
uses the native question-rejection endpoint. No Always Allow option or default
approval exists, and Hive does not call a mode setter or claim Build before
provider evidence. Request-changes text is not part of this native contract.

Cancellation, timeout, unavailable UI and invalid responses never send Yes.
Cancelled identities are suppressed for the attachment; disconnect/takeover
can transfer a waiting confirmation to the current pilot. Stream loss withdraws
the old attempt. Reconnect and ambiguous HTTP failure require successful native
pending-state reconciliation and a fresh confirmation. Epoch/generation and
request identity are rechecked at publication admission. A cancellation winning
before admission prevents dispatch; a dispatched HTTP request cannot be
retracted. Native clients can independently answer and are not locked out.

Requests from other sessions are never promoted to a parent. Directly attached
child-session plan exits are rejected with a controlled diagnostic. Replay
suppression is bounded to 1024 tool-linked question identities per attachment;
exhaustion fails closed and asks the user to reconnect. This behavior has
deterministic fake-server and TUI/Web regression coverage, not a retained
real-provider inference probe. See the [OpenCode guide](agents/opencode.md#approve-leaving-plan-mode).

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
| `hive::opencode::plan_approval` | A child transition or exhausted replay capacity prevents approval; inspect the native session and reconnect or refine in the parent. |

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

[claude-acp]: https://github.com/agentclientprotocol/claude-agent-acp/blob/739dde3ae5bb848bdb90e2c9150e2cc4687d480c/README.md
[codex-acp]: https://github.com/agentclientprotocol/codex-acp/blob/a77829fecc87cebbb2e53782ce6795aee072adc8/README.md
[spec-pin]: https://github.com/agentclientprotocol/agent-client-protocol/tree/e2d83349bc9e28a4059ea3acefa1c91c966f74e2
[sdk-pin]: https://github.com/agentclientprotocol/rust-sdk/tree/305aefda6fbe88be6ae8b9088003e383f909b135
[sdk-v2]: https://github.com/agentclientprotocol/rust-sdk/blob/305aefda6fbe88be6ae8b9088003e383f909b135/md/protocol-v2.md
[sdk-http]: https://github.com/agentclientprotocol/rust-sdk/blob/305aefda6fbe88be6ae8b9088003e383f909b135/md/http-transport.md
[draft-pin]: https://github.com/agentclientprotocol/agent-client-protocol/tree/1c0fe8dd680aa6de119b2136a7e5495075baf257
[draft-comparison]: https://github.com/agentclientprotocol/agent-client-protocol/compare/e2d83349bc9e28a4059ea3acefa1c91c966f74e2...1c0fe8dd680aa6de119b2136a7e5495075baf257
[draft-init]: https://github.com/agentclientprotocol/agent-client-protocol/blob/1c0fe8dd680aa6de119b2136a7e5495075baf257/docs/protocol/v2/initialization.mdx
[draft-commands]: https://github.com/agentclientprotocol/agent-client-protocol/blob/1c0fe8dd680aa6de119b2136a7e5495075baf257/docs/protocol/v2/slash-commands.mdx
[draft-unstable]: https://github.com/agentclientprotocol/agent-client-protocol/blob/1c0fe8dd680aa6de119b2136a7e5495075baf257/schema/v2/schema.unstable.json
[draft-setup]: https://github.com/agentclientprotocol/agent-client-protocol/blob/1c0fe8dd680aa6de119b2136a7e5495075baf257/docs/protocol/v2/session-setup.mdx
[draft-config]: https://github.com/agentclientprotocol/agent-client-protocol/blob/1c0fe8dd680aa6de119b2136a7e5495075baf257/docs/protocol/v2/session-config-options.mdx
[draft-list]: https://github.com/agentclientprotocol/agent-client-protocol/blob/1c0fe8dd680aa6de119b2136a7e5495075baf257/docs/protocol/v2/session-list.mdx
[draft-tools]: https://github.com/agentclientprotocol/agent-client-protocol/blob/1c0fe8dd680aa6de119b2136a7e5495075baf257/docs/protocol/v2/tool-calls.mdx
[v1-methods]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v1/meta.json
[v2-methods]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v2/meta.json
[v1-schema]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v1/schema.json
[v2-schema]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v2/schema.json
[v1-unstable]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v1/schema.unstable.json
[v2-unstable]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v2/schema.unstable.json
[v1-init]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/initialization.mdx
[v2-init]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/initialization.mdx
[v1-setup]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/session-setup.mdx
[v2-setup]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/session-setup.mdx
[v1-auth]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/authentication.mdx
[v2-auth]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/authentication.mdx
[v1-list]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/session-list.mdx
[v2-list]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/session-list.mdx
[v1-delete]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/session-delete.mdx
[v2-delete]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/session-delete.mdx
[v1-prompt]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/prompt-turn.mdx
[v2-prompt]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/prompt-lifecycle.mdx
[v1-content]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/content.mdx
[v2-content]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/content.mdx
[v1-tools]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/tool-calls.mdx
[v2-tools]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/tool-calls.mdx
[v1-plans]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/agent-plan.mdx
[v2-plans]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/agent-plan.mdx
[v1-elicitation]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/elicitation.mdx
[v2-elicitation]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/elicitation.mdx
[v1-fs]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/file-system.mdx
[v1-terminals]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/terminals.mdx
[v1-cancel]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/cancellation.mdx
[v2-cancel]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/cancellation.mdx
[v1-config]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/session-config-options.mdx
[v2-config]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/session-config-options.mdx
[v1-modes]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/session-modes.mdx
[v1-commands]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/slash-commands.mdx
[v2-commands]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/slash-commands.mdx
[v1-errors]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/error.mdx
[v2-errors]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/error.mdx
[v1-ext]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/extensibility.mdx
[v2-ext]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/extensibility.mdx
[v1-transports]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v1/transports.mdx
[v2-transports]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/transports.mdx
[manifest]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/Cargo.toml
[daemon]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-daemon/src/lib.rs
[transcript]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-daemon/src/transcript.rs
[plans]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-daemon/src/plans.rs
[activity]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-daemon/src/activity.rs
[interactions]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-daemon/src/interactions.rs
[config]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-daemon/src/session_config.rs
[journal]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-daemon/src/prompt_journal.rs
[persistence]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-daemon/src/persistence.rs
[gateway-tests]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-daemon/tests/gateway.rs
[compat-tests]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-daemon/tests/compat.rs
[tui-state]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-tui/src/state.rs
[tui-ui]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-tui/src/ui.rs
[tui-transcript]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-tui/src/transcript.rs
[tui-interactions]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/crates/hive-tui/src/interaction_tests.rs
[web-client]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/clients/hive-web/src/client.js
[web-ui]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/clients/hive-web/src/App.vue
[web-tests]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/clients/hive-web/app.test.js
[web-component-tests]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/clients/hive-web/src/App.test.js
[web-markdown-tests]: https://github.com/noirbizarre/hive/blob/f9e6a4108b8ca885a0f58dbccd9e4dd92e5ccc17/clients/hive-web/src/MarkdownMessage.test.js

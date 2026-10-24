# Troubleshooting

Start with `hive doctor` or `hive doctor <profile-id>`. Match the printed code
to the action below. For a custom setup, pass the same `--config` and `--listen`
used by the daemon.

## The client cannot reach Hive

1. Start `hive daemon` in another terminal.
2. Run `hive daemon status`.
3. Confirm client and daemon addresses match.

For port 9000, use `hive tui --address 127.0.0.1:9000` but
`hive web --url http://127.0.0.1:9000`. The TUI does not read `HIVE_URL` or
`HIVE_LISTEN`. A web command opening a page is not a daemon health check.

`hive::doctor::port_in_use` means another process holds the requested port.
`hive::doctor::daemon_locked` means a daemon holds the runtime lock but did not
answer at the checked address. Find that daemon's actual address before trying
to start another. A different port does not bypass the singleton lock.

## No profiles, wrong profile, or invalid configuration

| Code | What to check |
| --- | --- |
| `hive::config::unreadable` | Explicit file path exists and is readable |
| `hive::config::syntax` | Valid TOML, no unknown fields, reported line |
| `hive::config::default_agent` | Default is a configured profile ID, not its display name |
| `hive::config::profile` | Exactly one valid command or endpoint shape |
| `hive::doctor::unknown_agent` | Selected ID matches `[agents.<id>]` |
| `hive::doctor::no_agents` | Create the configuration or select the intended file |

An absent implicit default file is allowed and yields no agents. An explicit
missing file fails. Restart the daemon after edits: configuration is not hot-reloaded.
See [configuration](configuration.md).

## Agent initialization fails

| Code | Action |
| --- | --- |
| `hive::compat::binary_missing` | Install the executable or use its absolute path |
| `hive::compat::pi_without_adapter` | Use `pi-acp`, not bare Pi or Pi RPC |
| `hive::compat::protocol_mismatch` | Match `versions` to the installed ACP wire version |
| `hive::compat::missing_capability` | Check the agent supports required session operations |
| `hive::compat::startup_timeout` | Run the agent command manually and inspect its startup |
| `hive::compat::initialize_failed` | Check entrypoint, protocol, agent logs, and prerequisites |
| `hive::compat::endpoint_unreachable` | Start the endpoint service and check its address |

Keep arguments separate in `args`. Do not put a whole shell command in `command`.
ACP stdout must contain protocol traffic, not startup banners. Configure provider
credentials in the environment/configuration used by the daemon's agent process.

Doctor's initialize-only probe can succeed while actual session creation or
prompting fails. It does not test model credentials or every recovery capability.
Initialization errors do not trigger automatic v2-to-v1 fallback.

## Permissions or state prevent startup

For `hive::doctor::unsafe_runtime_dir` or `hive::doctor::unsafe_state_dir`, inspect
the reported location's ownership and access permissions. Existing shared or
world-readable locations can be refused. Keep Hive's runtime/state private.

Doctor and actual startup have some different path checks, so inspect the exact
daemon error as well. See [storage and backups](operations.md#storage-and-backups).

## Prompting is disabled or control was lost

Select the original session and take control (`Ctrl-T` in the TUI, or
type in the browser composer). Wait until the daemon confirms pilot ownership.
An observer cannot mutate the session, and another client's takeover fences
the previous pilot.

Wait for idle before submitting a second distinct prompt. Acknowledgement is
not completion. `hive::opencode::externally_controlled` means a native OpenCode
client is working; wait for it to finish or cancel deliberately as pilot.

## History is missing or a prompt outcome is uncertain

The in-memory replay window is bounded to 256 updates and is lost on daemon
restart. Accept live-only attachment explicitly when you no longer need the
missing messages. Current activity can be restored, but missing text cannot.

Retry an uncertain prompt only through a client's explicit retry action retaining
the original key. After restart, inspect interrupted work and use the separate
recovery acknowledgement action before sending anything new. See
[sessions and handoff](guides/sessions.md#uncertain-prompts-and-interrupted-work).

## OpenCode server or adoption fails

| Code | Action |
| --- | --- |
| `hive::opencode::missing_credentials` | Set the variable named by `password_env` in the daemon environment |
| `hive::opencode::unauthorized` | Match credentials to the running server |
| `hive::opencode::unsupported_version` | Use a supported server and check the recorded version matrix |
| `hive::opencode::directory_mismatch` | Use the native session's exact project directory |
| `hive::opencode::already_adopted` | Open the returned existing Hive session ID |
| `hive::opencode::session_not_found` | Verify the native session still exists in the pinned API store |

Native adoption needs both `agent` and `adopt` metadata. An OpenCode REST server
is not an ACP endpoint. See [OpenCode integration](agents/opencode.md).

## The TUI leaves its screen but does not return to the shell

After Esc closes the interface, the process can still be waiting for its
blocking terminal-input reader to finish. Press Enter once to deliver the final
input event and let it exit. The session has detached and remains in the daemon.

This workaround was observed in the isolated mock-agent quickstart check.

## Reporting a problem

Include Hive/agent versions, transport, selected profile ID, diagnostic code,
and the failing step. Keep initialization, prompt, recovery, and native-adoption
results distinct. Review logs before sharing: configuration headers, environment
values, prompt content, local paths, and control tokens may be sensitive.

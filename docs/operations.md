# Operating Hive

## Daemon lifecycle

Run `hive daemon` in a terminal and leave it running. Use `hive daemon status`
from another terminal to query its JSON status. Hive does not provide background
start, stop, or restart subcommands.

Interrupt the foreground process with `Ctrl-C` to stop it. Client exit is
different: it leaves downstream work running while the daemon remains alive.

Only one daemon can hold the per-user runtime lock. Changing its port does not
allow a second daemon in the same runtime namespace. The lock is released on
process exit. See [configuration paths](configuration.md#runtime-locations).

## Reloading configuration

```sh
hive daemon reload
hive daemon reload --url http://127.0.0.1:9000
# Unix: substitute the PID of the running Hive daemon.
kill -HUP "$HIVE_PID"
```

Both triggers reread the daemon's original configuration file. A command prints
the accepted catalog revision or exits unsuccessfully with a diagnostic. SIGHUP
logs the outcome and keeps listening after a failed reload. A timed-out command
may still have applied its candidate; check status or safely retry.

Configuration is published only after complete validation. Existing sessions
continue unchanged; new sessions use updated profiles and defaults. Open clients
refresh provider choices. Invalid, missing, or unreadable files leave the previous
configuration active. Reload does not launch probes or retry offline recovery.
See [reload semantics and identity conflicts](configuration.md#reloading-changes).

## Health and diagnosis

```sh
hive daemon status
hive doctor
hive doctor opencode --json
```

Status reports the daemon's view of sessions and configured agents. Health and
readiness mean the HTTP listener responds, not that a model or agent is usable.
Provider discovery reports declarations, not successful live probes.

Doctor checks configuration, profile initialization or server health, listen
address, lock, and storage permissions. It sends no model prompt. Profile
processes are still launched, and an `npx` profile may need to download its adapter.

Doctor is a diagnostic aid rather than an exact replica of startup. Its probe
timeouts, executable lookup, and some state-path checks differ from session
startup. A profile `env.PATH` override is not fully modeled by preliminary
executable checks. Test a real session after initialization succeeds.

Use [troubleshooting](troubleshooting.md) for diagnostic actions and
[CLI reference](reference/cli.md) for non-default paths/addresses.

## Storage and backups

Hive stores its catalog in `$XDG_STATE_HOME/hive/sessions.sqlite3`, falling back
to `$HOME/.local/state/hive/sessions.sqlite3`. It uses SQLite WAL.

Persisted data includes session identities, downstream IDs, profile/adapter
choices, titles, usage bookkeeping, and a minimal request journal. The journal
holds request keys, versioned SHA-256 content fingerprints, and acknowledgements
(message IDs) or failure markers. It does not hold prompt bodies, arbitrary
acknowledgement metadata, downstream error text, or assistant transcripts.
Titles and other registry metadata can still contain user-derived text;
fingerprints are not encryption. Keep the database and backups private.
Control tokens, conversation replay payloads, and activity snapshots are volatile.

The registry can list sessions without a local transcript or an available agent.
The prompt journal is required for idempotency and interrupted-work protection;
it is not a reconstructable assistant transcript. Hive maintains no durable
transcript archive. A future optional archive requires an explicit purpose and
retention/fallback policy; see [ADR-009](adr/009-agent-owned-history-and-session-registry.md).
Schema version 2 upgrades earlier journals transactionally: it fingerprints each
legacy prompt, blanks its body, and reduces results to message IDs or failure
markers without releasing pending reservations. Invalid legacy rows abort the
upgrade rather than dropping requests. Older Hive binaries refuse schema 2.
This removes bodies and arbitrary result payloads from logical journal columns;
old pages/WAL files and backups may still contain previous data. Migration does
not compact the database or delete backups.

On Unix, runtime/state directories and files must be private. Hive creates
private locations but can refuse existing unsafe ones rather than repair them.
Check ownership and permissions before moving state. Do not remove another
process's live lock or database to resolve a startup error.

For a filesystem backup, stop Hive first and copy the database together with any
remaining WAL file. Alternatively, use SQLite's backup API. A newer unsupported
database schema is refused rather than overwritten.

Explicit close removes a session record. Orphaned/lost records without uncertain
work are eligible for pruning after 90 days without activity. Unavailable records
and interrupted work remain until resolved.

## Restart and recovery

At startup, Hive attempts to reattach saved downstream IDs. It does not silently
create replacement conversations or resend interrupted prompts. Recovery depends
on the agent retaining the session and exposing the necessary recovery method.

| State | Meaning | Action |
| --- | --- | --- |
| `recovering` | Reattachment in progress | Wait and refresh |
| `reattached` | Original downstream session reattached | Inspect state and continue if usable |
| `unavailable` | Agent/server could not currently be reached | Fix availability/configuration and inspect the recovery reason |
| `orphaned` | Required attachment capability/API is unavailable | Restore a compatible agent/API |
| `lost` | Downstream no longer recognizes the saved session | Inspect native storage; Hive will not replace it automatically |
| `recovery_required` | Reattached with uncertain interrupted work | Inspect the agent, then explicitly acknowledge before new work |

The catalog distinguishes worker lifecycle from recovery status. See the
[exact fields](hive-api.md#session-catalog) when integrating clients.

Replay and activity snapshots do not survive restart. A client may need to
accept live-only attachment. That acceptance does not acknowledge interrupted
work: use the separate TUI `Ctrl-Y` or browser recovery action after inspecting
what the agent actually did.

The catalog's `transcript` object reports history independently of recovery.
ACP restoration uses negotiated protocol support, never agent identity. Available
retained history may be partial or have unknown coverage; both clients keep that
status visible while rendering it. An empty successful replay is accepted with a
warning when earlier completed work is recorded. An agent outage does not produce
an offline transcript: fix availability and restart Hive to retry restoration.

Acknowledgement permits new prompt keys. It neither resends the interrupted key
nor proves its effects completed.

## Usage and costs

Hive records reported turn token totals and cumulative session cost when the
agent supplies them. Missing values remain unknown. The context-window gauge
is not billable usage, and turn aggregates are not individual model-call records.

Optional models.dev mappings produce estimates. Price lookup failure does not
block token tracking. Provider-reported costs remain separate from estimates.
See [pricing configuration](configuration.md#pricing) and the
[usage reference](acp-compatibility.md#token-usage-and-costs).

## Trust boundary

Hive serves one trusted local OS user. Its daemon listener is loopback-only
and has no per-client login. Other local processes able to reach it can access
sessions and administration. Pilot tokens fence stale clients; they do not
create a multi-user authentication boundary.

Downstream endpoints have separate policies. OpenCode server credentials let
Hive act on that server, and discovery can reveal native titles/directories.
Keep profile secrets and local state private. See
[architecture](architecture.md#trust-boundary) for the boundary rationale.

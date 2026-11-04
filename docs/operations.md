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

### Project definitions

`hive project list` shows both configured and managed definitions. Managed
registrations and node-local directory overrides persist in the same private
database as the session catalog; include them in backups. Configured definitions
remain in their TOML files and must be backed up separately.

Use TOML/reload for configured entries and `hive project update` for managed ones.
ID collisions reject startup or reload. Removing a definition only detaches
metadata: it never removes a checkout, workspace, local directory, or override.
See [project commands](reference/cli.md#projects).

### Repositories and task workspaces

`hive repository list` reports the stable local node ID and reusable cache status;
`hive repository fetch <id>` refreshes one cache explicitly. Configure private
repository access through node-local Git credential helpers or SSH credentials.
Hive disables interactive credential prompts and retains controlled errors rather
than raw command stderr. Repository URLs must remain credential-free.

`hive workspace list`, `show <id>` and `inspect <id>` expose durable lifecycle,
backend, actual paths and fresh cleanup evidence. `hive workspace create --project
hive --policy task` provisions independently of sessions. Commands print operation
IDs before network I/O; preserve the key with `--operation <id>` on identical
retries. `hive workspace operation <id>` shows original intent, pinned commit,
partial path, hook snapshot and session holds. Branch/ref/path conflicts never
force checkout or adopt an unrelated directory.

Resources reside beside the session database under `repositories/`, `clones/`,
and (for preserved independent-clone history) `archives/`. Newly acquired worktree
caches use `<project-root>/.git` with sibling checkouts. WorkTrunk can place its
checkouts elsewhere; include those actual paths in backups. Back up resource files
and the database together while Hive is stopped. Moving a bare repository without
repairing Git's linked-worktree metadata breaks its checkouts.

Resource snapshots pin separate repository-backend and workspace-provisioner
identities. On first access after upgrading, versionless records migrate atomically
without moving directories or rewriting original operation/sweep requests. Native
Git, clone and WorkTrunk records retain their implementation regardless of current
configuration. Missing implementations or unsupported backend-data versions require
restoring a compatible installation; they do not trigger native Git fallback.
Back up the database and resource files before upgrading. To downgrade, restore a
matching pre-upgrade backup rather than opening an upgraded snapshot with an older
binary. See [provenance migration](adr/011-project-and-workspace-domain.md#persisted-provenance-migration).

Removal is explicit: `hive workspace remove <id>`. It refuses active/unknown use,
unverifiable identity and dirty tracked, untracked or ignored content. Explicit
`--discard-changes` permits data loss only for an otherwise verified idle managed
resource. Worktree branches are retained; clean independent-clone removal first
archives all refs/objects, including unpushed commits, stashes and notes. Externally owned
directories cannot be deleted through workspace cleanup.

Disconnect, shutdown and transport failure retain session-use holds. Use
protection covers adopted native sessions and managed checkouts reached
through existing-directory aliases. Pending durable startups remain visible in
the session catalog even if their requesting RPC is cancelled. Confirmed
close releases a hold atomically with removal of the session catalog record,
without deleting its workspace. Restart validates the original workspace before
reattaching; missing/conflicting resources remain unavailable, never replaced.
Abandoned reservations can be released only when admission, durable records and
the dispatch journal together prove that no downstream worker could have started.
Inspect uncertain startup/cleanup and hook effects before retrying.

#### Recovery inventory and ephemeral sweeps

Before serving requests or recovering agent sessions, startup settles only proven
completed native creation/removal and initializes missing idle-retention metadata.
It never fetches, recreates checkouts, resumes deletion or applies saved sweeps.
WorkTrunk's uncertain hook effects still require explicit reconciliation.

`hive workspace recovery` reports incomplete operations, missing/conflicting paths,
repository staging, session holders, retention expiry and cleanup blockers. Recorded
lifecycle observations carry operation IDs and Unix-second timestamps; old records
start their history when first observed, without inventing earlier transitions.
Orphan discovery scans fixed Hive directory levels (up to 4096 entries) and known
repositories' worktree registrations, including WorkTrunk paths. Scan failures and
truncation are reported. Unrecorded resources, missing project associations and
unreferenced workspaces are inspection candidates, never deletion authority.

Ephemeral expiry makes a resource eligible for an **explicit** sweep:

```sh
hive workspace recovery
hive workspace sweep plan
hive workspace sweep show <operation-id>
hive workspace sweep apply <operation-id>
```

Planning prints a durable operation ID before contacting the daemon and freezes
the current selection, per-workspace cleanup IDs and complete preserve-changes
requests. `--workspace <id>` may be repeated to select specific candidates;
`--operation <id>` retries identical planning intent. Newly eligible resources
require a new plan. Apply retries the same plan and reports every item's result;
unresolved items make the CLI exit nonzero. Dirty, active, uncertain, unverifiable,
persistent and external resources remain protected. Interrupted provisioning must
be explicitly reconciled to a verified ready resource before a sweep can remove it.
An uncertain WorkTrunk removal requires hook reconciliation before retrying.

`until` retention uses its configured Unix-second expiry. `idle` uses the later of
configured `last_used_at` and runtime use, refreshed at readiness, successful
admission and confirmed final session release, including existing-directory aliases.
Unknown session holds still block deletion even after their catalog rows disappear.
Legacy records get one conservative full idle window on first observation; overflow
retains resources, and recorded timestamps never regress on clock rollback.

Every removal rechecks eligibility and fresh ownership, cleanliness and use under
the resource gate; the gate coordinates Hive operations, not arbitrary external
filesystem writers. Sweeps never discard dirty work, delete repository caches or
archives, or delete a workspace just because a prompt/session ended. Worktree
branches and independent-clone archives retain the normal removal guarantees.
Activity-marker aggregation and stale-marker recovery belong to #118.

For interrupted checkout creation, `hive workspace reconcile <id>` validates
the recorded Git identity without recreating it. If WorkTrunk's path response
was lost, supply its actual directory with `--directory`. For uncertain hooks,
review and reconcile their external effects first, then use `--hooks-reconciled`.
This never replays provisioning hooks. Cleanup reconciliation either verifies
completed absence or permits replay of the original removal operation; safety
checks still apply. A native cleanup whose response was lost can also reconcile
absence when retried with its original operation ID.

Archive plans are recorded before copying an independent clone. Partial copies
and completed archives are reconciled against the original refs before removal,
including interruption between copying, renaming and recording the final path.
Timed-out Git/WorkTrunk commands terminate their process group (Windows Job) and
cancel output collection; successfully detached WorkTrunk hooks retain their
normal background lifetime.

### Database contents

Hive stores its catalog in `$XDG_STATE_HOME/hive/sessions.sqlite3`, falling back
to `$HOME/.local/state/hive/sessions.sqlite3`. It uses SQLite WAL.

Persisted data includes session identities, downstream IDs, profile/adapter
choices, titles, usage bookkeeping, and a minimal request journal. The journal
holds request keys, versioned SHA-256 content fingerprints, and acknowledgements
(message IDs), dispatch/terminal evidence and recovery acknowledgement markers.
It does not hold prompt bodies, arbitrary
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
upgrade rather than dropping requests. Schema 4 adds managed project definitions
and node-local overrides without cascading resource deletion. Schema 5 adds the
durable local node, repositories, workspaces, operation history and session-use
holds. Schema 3 adds write-ahead dispatch,
controlled terminal evidence and per-request recovery acknowledgements. Older
Hive binaries refuse unsupported schemas. Legacy null/failed outcomes cannot
prove non-dispatch: upgrading may require a one-time explicit acknowledgement,
even if an older daemon had cleared its session uncertainty flag.
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
nor proves its effects completed, stopped, or are fully represented in history.
Running or blocked work still prevents concurrent new prompts. Both clients can
acknowledge without replaying history; skipping missing history is a separate
choice. The TUI reconnects as an observer; take control with `Ctrl-T` and use
`Ctrl-L` to explicitly retrieve a pending receipt with its original key/content.
The browser provides **Retrieve original receipt**. Neither client automatically
resubmits a pending prompt after reconnect.

### Prompt evidence and recovery actions

| Evidence | What Hive can establish | Recovery behavior |
| --- | --- | --- |
| New-format reservation before dispatch marker | Hive did not publish this attempt | Keep key reserved; submit new intent with a new key |
| Dispatch marker without a receipt | Acceptance/execution may have happened | Keep uncertain; inspect and acknowledge before new work |
| Recorded `messageId` | Original acknowledgement is available | Same-key lookup returns it; it does not prove completion |
| Correlated live running/idle and recorded receipt | Foreground termination was observed | Retain controlled terminal reason, if supplied |
| Replayed messages or current idle, without reliable prompt correlation | Session/history state only | Do not reconstruct the original result or clear uncertainty |

A crash between the dispatch marker and actual publication remains conservatively
uncertain. Generic transport failures after possible publication are also
uncertain, rather than proof that the prompt failed to execute. A later idle
cannot repair a missing receipt. Native OpenCode idle is not reported as successful
completion merely because no failure was observed.

Request errors carry versioned structured guidance in `error.data.hive`; see
[the diagnostic contract](compatibility.md#prompt-recovery-diagnostics).
An acknowledged recovery incident remains acknowledged across another restart,
while a newly lost outcome raises a new fence. Do not delete journal reservations
to retry work: that would remove duplicate-execution protection.

## Usage and costs

Hive records reported turn token totals and cumulative session cost when the
agent supplies them. Missing values remain unknown. The context-window gauge
is not billable usage, and turn aggregates are not individual model-call records.

Optional models.dev mappings produce estimates. Price lookup failure does not
block token tracking. Provider-reported costs remain separate from estimates.
See [pricing configuration](configuration.md#pricing) and the
[usage reference](compatibility.md#token-usage-and-costs).

## Trust boundary

Hive serves one trusted local OS user. Its daemon listener is loopback-only
and has no per-client login. Other local processes able to reach it can access
sessions and administration. Pilot tokens fence stale clients; they do not
create a multi-user authentication boundary.

Downstream endpoints have separate policies. OpenCode server credentials let
Hive act on that server, and discovery can reveal native titles/directories.
Keep profile secrets and local state private. See
[architecture](architecture.md#trust-boundary) for the boundary rationale.

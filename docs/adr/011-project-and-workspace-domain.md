# ADR-011: Project and workspace domain contracts

## Status

Accepted. Defines the contracts for [issue #83](https://github.com/noirbizarre/hive/issues/83).
Concrete node-local backends, repository caching and session-start integration
implement these contracts in the daemon workspace service (#85/#86/#87).

## Context

A logical project may have repositories on several execution nodes and several
task workspaces on each node. A path identifies none of these portably. Session
shutdown must not remove a workspace or erase uncommitted work. Provisioning can
also fail after creating a branch or directory, making blind retries unsafe.

The domain must support existing directories, native Git, and optional WorkTrunk
without embedding Git mechanics in the broker or ACP transport. Worktrees isolate
checkouts; they are not a security sandbox.

## Decision

### Resource identities and locations

`hive-core` owns the transport-independent types and checked workspace lifecycle.
Its existing session vocabulary remains separate from workspace state.

| Resource | Identity and lifetime |
| --- | --- |
| `Project` | Portable logical identity, display name, optional repository source; independent of configuration files and paths |
| `NodeRepository` | One reusable checkout or cache belonging to a project on one node |
| `Workspace` | Concrete environment with project, owning node, policy, retention, current operation identity, and observed location |
| `OperationId` | Durable key for one logical provision or cleanup operation, including retries |

Project IDs use lowercase slugs, with numeric suffixes allocated at registration
when a slug is taken. Existing `project-<UUID>` IDs remain valid. Node, repository,
workspace, and operation IDs have distinct prefixes and canonical UUID spellings.
Allocate once and persist them. In particular, node
identity comes from node registration, not from a new allocation per request.
Renaming a project or moving a directory does not change its identity.

| Field | Portability and authority |
| --- | --- |
| Resource IDs | Portable identifiers; project slugs originate from names at registration, never regenerated from cwd or configuration filenames |
| Repository source | Logical locator resolved by the owning backend; not a credential store |
| `NodePath` | Absolute node-local path with explicit owning node and POSIX/Windows syntax |
| Requested base ref and branch | Portable intent; backend resolves and validates against the actual repository |
| Observed root and effective working directory | Node-local backend observations; may differ when running from a subdirectory |
| Resolved base, branch, and commit | Observations to persist rather than reconstruct from requested intent |
| Backend identity | Opaque provenance used to route future inspection and cleanup |

Core validates absolute path syntax without consulting the initiating machine's
filesystem. The owning backend validates existence, symlinks, containment, full
Git ref/branch syntax, repository membership, and source availability. A path on a
remote node must not be interpreted as a local filesystem path. Repository IDs in
worktree requests must resolve to the same project and execution node.

### Workspace policies and ownership

| Policy | Required intent | Ownership |
| --- | --- | --- |
| `existing` | Explicit existing directory on the execution node | External |
| `worktree` | Node repository ID, base ref, optional branch | Hive-managed |
| `clone` | Base ref and optional branch; source resolved from project | Hive-managed |
| `ephemeral` | Explicit worktree or clone backing, with expiry or idle retention | Hive-managed |

When a managed branch is omitted, `GitIntent::effective_branch` supplies a
task-specific branch derived from the workspace ID. Retries retain that name.
The backend still rejects branch collisions; it cannot silently reuse an unrelated
branch. Existing directories remain usable without inventing a Git remote.

Ephemeral describes lifetime intent, not a fifth Git mechanism. Explicit backing
prevents a backend change from silently changing isolation behavior. Retention is
modeled independently as retain-until-explicit-removal, an expiry timestamp, or an
idle period. Timestamps use Unix seconds. Eligibility does not authorize removal,
and ending or disconnecting a session does not trigger a lifecycle transition.

### Lifecycle

`Workspace` encapsulates mutable state. Events come from orchestration after
backend results have been correlated with the current workspace and operation.
Deserialization checks state-dependent invariants, including node affinity,
required operation IDs, ready locations, and failure retry dispositions.

| Current state | Event/condition | Next state |
| --- | --- | --- |
| `requested` | Start provision with durable operation ID | `provisioning` |
| `provisioning` | Provision succeeds with observed location | `ready` |
| `provisioning` | Provision fails, retaining partial observations and retry disposition | `failed` |
| `failed` | Safe retry using the same operation ID | `provisioning` |
| `ready` or safely reconciled `failed` | Start cleanup with a new operation ID | `cleaning` |
| `failed`, including manual-only failure | Explicit inspection/manual reconciliation authorizes abandoning provision and cleaning its partial resource with a new operation ID | `cleaning` |
| `cleaning` | Removal or verified absence | `removed` |
| `cleaning` | Failure with retry disposition | `cleanup_failed` |
| `cleanup_failed` | Safe retry with the same cleanup operation ID | `cleaning` |
| In-flight or failed provision/cleanup | Inspection proves same-operation retry is safe | Corresponding failure state, marked safe to retry |
| `provisioning`, `failed`, or `cleanup_failed` | Inspection proves resource usable, with observed location | `ready` |

All other transitions fail without changing the record. `removed` is terminal.
Interrupted in-flight work requires inspection before resumption. A manual-only
failure cannot be upgraded to retryable by the retry-reconciliation event; explicit
inspection may establish an already usable resource instead. Reconciliation never
means allocating a replacement with a different identity.

### Backend interface and retry contract

`WorkspaceBackend` provides object-safe `provision`, `inspect`, and `cleanup`
methods returning boxed `Send` futures at the service boundary. Its concrete
implementation is shared orchestration, composed from two independent contracts:

- `RepositoryBackend` acquires and updates storage, verifies its identity and
  inspects backend-specific revisions. `RepositoryHandle` pins the repository ID,
  project, owning node, `RepositoryBackendId`, `StorageKind` and versioned metadata.
- `WorkspaceProvisioner` prepares, creates, inspects, reconciles and removes working
  environments. `ProvisionerId` is independent of the repository backend ID.
  Descriptors declare accepted backend/storage pairs and capabilities for linked
  workspaces, independent checkouts, uncertain hooks and activity reporting.
- `BackendManager` resolves compatible pairs and dispatches recorded identities.
  Compatibility and payload validation precede acquisition or checkout effects.
  Unknown implementations, storage kinds or payload versions fail explicitly;
  they never select a fallback during recovery or cleanup.
- The workspace service retains operation history, state transitions, persistence,
  admission, session holds, retention and cleanup policy. Implementations receive
  immutable inputs and a narrow checkpoint writer, not the service's store or
  lifecycle state. A failed durable barrier prevents the next effect.

Core chooses neither an executor nor a subprocess implementation. Synchronous
implementation contracts run inside the daemon's admitted blocking job, without
re-entering its cross-process gate. Requests retain node affinity and operation
keys. Commands and filesystem inspection execute on the owning node.

The built-in Git repository backend is `git`, with storage kind `git.bare-cache`.
Provisioners `git.worktree`, `git.clone` and `worktrunk` all consume the same Git
repository handle; WorkTrunk is explicitly Git-only, not a separate SCM.
`external.directory` requires no repository and cannot remove directories.
Repository layout/registration discovery is implementation-provided read-only
evidence and never grants authority over an orphan.

New managed policies carry a `BackendSelection` and `BackendData` intent envelope
(`schema`, `version`, `data`). Preparation pins backend-specific resolution in
durable checkpoints; observations use the same envelope. Generic lifecycle code
does not require Git branches or worktrees. Git-specific shorthand and legacy
response projections are decoded at the Git boundary. A future SCM can register
its own storage/provisioner pair and codecs without changing the state machine or
emulating Git. Native Jujutsu uses repository backend `jj`, storage kind `jj.git`,
and provisioner `jj.workspace`. Portable project `scm` selection defaults to Git;
JJ's `default_revision` and `jj` policy remain independent of Git-only WorkTrunk
selection. JJ's initial backing store is its explicit Git interoperability model,
acquired non-colocated and operated through native JJ workspace commands.

JJ ownership evidence combines stable Hive repository/workspace/operation IDs,
native workspace registration and root, shared repository identity, and markers
inside `.jj`. Revision resolution pins both a JJ change ID and immutable commit ID.
Read-only inspection compares the current JJ tree with physical files without
snapshotting, including ignored/untracked data. Nonempty working-copy changes remain
dirty after snapshotting. Cleanup retains history with a stable Hive bookmark and
checkpoints preservation before deleting files or forgetting registration.

The provisioner contract supports checkpointed removal and explicit completion of
native deregistration after filesystem absence. Only a matching cleanup operation,
durable preservation proof, exact native identity and no usage holds authorize that
completion; startup recovery never dispatches it. Partial or conflicting state
remains uncertain. Federation startup transport remains separate work (#88).

Before external effects, persist the operation ID, both selected identities, the
compatible repository handle/reference, and
complete intent. Workspace records retain backend identity even before a path is
known; retries and reconciliation cannot switch integrations. Repeating
the same ID and intent reconciles the same resource. Changing intent under an
existing key, or reusing a provision key for cleanup, returns `RetryConflict`.
The workspace stores the current operation identity; durable request history and
cross-operation-key conflict detection belong to backend/orchestration persistence.
Replay a cleanup's original durable request verbatim, including its location
snapshot. If inspection discovers a previously unknown location, use that fresh
observation internally by workspace identity; do not rebuild the retry request with
different observational hints under the same key.
Provision distinguishes created from already satisfied; cleanup distinguishes
removed from already absent. Read-only inspection distinguishes present, partial,
absent, and identity conflict. Absence must be verified by identity, not inferred
from a missing path guessed from a template.

Retry dispositions are `same_operation`, `inspect_first`, and
`manual_intervention`. Interrupted commands and lost responses can have uncertain
effects. Integrations with externally visible hooks may be non-idempotent; they
must report uncertainty or manual reconciliation instead of promising exactly-once
execution. Partial resource observations survive failure for later inspection.

Backend results must persist the effective root, cwd, and backend provenance before
publishing readiness. Later configuration changes cannot reinterpret that identity
or route removal to an unrelated backend.

#### Persisted provenance migration

The resource snapshot has an explicit schema version. Versionless snapshots migrate
atomically under resource admission to version 1 before further effects. Legacy
`native`, `clone`, and `worktrunk` identities map deterministically to `git` plus
`git.worktree`, `git.clone`, and `worktrunk`; `existing` maps to
`external.directory` without a fabricated SCM. Legacy Git policy records remain
decodable. Existing IDs, repository layouts, actual paths, partial observations,
resolved commits, uncertainty markers, archives and session holds survive migration.
When a legacy WorkTrunk response was lost before its path was recorded, migration
retains a pathless checkpoint. Explicit hook acknowledgement and repository/branch/base
verification bind the supplied observed directory; recovery never guesses a path or replays hooks.

Immutable submitted request JSON is separate from resolved execution checkpoints.
Migration does not rewrite original create/cleanup requests or frozen sweep
requests, so unchanged retries still compare identically and changed snapshots
still conflict. Explicit operation kinds replace request-field heuristics after
legacy decoding. Configuration changes and auto-detection never reselect a pinned
implementation. Unsupported future snapshot versions are rejected. Downgrading
an upgraded snapshot requires restoring a matching pre-upgrade database/filesystem
backup; older binaries do not understand the new provenance contract.

### Cleanup safety and diagnostics

Cleanup obtains fresh ownership, cleanliness, and active-use evidence under the
same mutation guard used for deletion. Serialized inspection evidence and
caller-supplied paths are not deletion capabilities. Preserve dirty resources by
default, including untracked files. Refuse unknown cleanliness, active or unknown
use, unverifiable ownership, node mismatch, and identity conflicts. Explicit discard
intent permits dirty managed-resource removal but does not override other checks.
External resources cannot be deleted through this interface; detaching their Hive
records is a separate registry concern.

Recovery/retention (#89) uses a non-destructive startup pass and inspectable orphan
inventory. Expiry selects ephemeral cleanup candidates; only explicit sweep
application executes deletion. Plans persist complete immutable cleanup requests
and per-resource operation IDs before effects, then recheck safety under resource
admission on every attempt. Persistent resources remain explicit-remove only.
Runtime idle-use timestamps are separate from immutable provisioning intent and
refresh on readiness, admission and confirmed final release, including aliases.
Unknown dispatched holds survive restart/catalog pruning. WorkTrunk activity
publication, aggregation and stale-marker recovery remain wholly #118 work.

Typed errors distinguish unknown projects, unavailable nodes, invalid paths/refs,
provisioning and ordinary cleanup failures, unsafe cleanup, invalid transitions/records, conflicts,
uncertainty, and non-idempotent operations. Controlled reasons avoid exposing raw
subprocess output or credentials. Stable `hive::workspace::<kind>` codes and retry
help can be adapted into binary-edge diagnostics without coupling core to miette.

### Global WorkTrunk integration contract

The daemon composes the following global runtime configuration, independently
of agent profiles and workspace-policy declarations:

```toml
[worktrunk]
enabled = true
```

| Setting | Selection on the workspace-owning node |
| --- | --- |
| `true` | Require compatible, usable `wt`; actionable error, no native fallback |
| `false` | Use native backend; do not probe or invoke WorkTrunk |
| Omitted, including an absent or empty section | Auto-detect usable WorkTrunk, otherwise native backend |

`WorktrunkConfig::select` short-circuits explicit disablement before invoking its
probe callback. Detection considers capabilities, not merely executable presence.
Selection governs lifecycle delegation now; Hive activity publication will consume
the same global selection in #118. It is never a per-profile or per-file switch.

WorkTrunk-specific commands, hook handling, marker formats, and path templates stay
behind the integration boundary. Creation/removal must delegate to WorkTrunk when
selected. Discover its effective path and persist it; never infer it from Hive's
own template. Commands and markers execute on the workspace-owning node.

Activity publication must aggregate sessions sharing a worktree: one session ending
cannot clear another session's activity. The integration must document mappings for
running, waiting/needs-input, completed, cancelled, and failed activity and reconcile
stale markers after restart or lost sessions. Marker mappings and stale-marker
recovery are #118 work. Lifecycle commands, configuration composition and backend
pinning across reload are implemented by the workspace service.

### Implementation boundaries

The project registry (#84), repository operations (#85), concrete lifecycle (#86),
session startup (#87), remote routing (#88), recovery/retention execution (#89), and
WorkTrunk integration (#118) consume these contracts. Configuration fragments and
named profiles are composed independently (#117); no profile, context, or UI state
is attached to individual files in this domain.

## Consequences

### Project registry authority

Issue #84 implements config-owned definitions and a separate SQLite-managed
registry. TOML keys are explicit `ProjectId`s and never derived from fragment
filenames. Duplicate IDs across fragments or ownership sources are errors.
Existing agent profiles can reference configured projects after composition;
SQLite-only entries do not satisfy declarative references, keeping doctor and
configuration validation independent of node-local mutable state.

HTTP/CLI writes mutate managed definitions only. Configured entries require an
edit and reload. Both mutations and reload share a serialized publication gate;
durable writes commit before publishing the managed snapshot and catalog revision.
Registration validates portable metadata without Git or filesystem operations.
Node-local directory overrides are keyed separately by project and node identity,
and remain inert when their definition disappears. Removing definitions does not
authorize deletion of repositories, workspaces, or local directories.

### Domain tradeoffs

Three resource identities require explicit association and separate persistence,
but avoid confusing a shared cache, task checkout, and logical project. A simple
cwd-only or session-owned model cannot represent their different lifetimes safely.

Explicit backend provenance and observations cost metadata, but permit cleanup and
recovery without recalculating integration-specific paths. Treating all failures
as retryable would be simpler but could repeat hooks or create duplicate resources.

Core tests cover lifecycle transitions, record validation, cleanup checks, retry
contracts with a deterministic fake backend, and global integration selection.
The daemon tests concrete Git acquisition, concurrency, interrupted acquisition,
durability, safe cleanup and WorkTrunk path/hook delegation against local remotes.

### Flat bare acquisition and worktree-local configuration

New managed worktree repositories prefer `<project-root>/.git` as their bare
common repository, with native checkouts as immediate sibling directories.
Acquisition publishes the bare repository before adding linked checkouts, avoiding
administrative links to temporary staging paths. Existing layouts are preserved.
WorkTrunk chooses its own paths without a Hive target-directory override.

Acquisition enables `extensions.worktreeConfig`, scoping `core.bare=true` to the
bare repository's `config.worktree` rather than shared config. Worktree-specific
hook paths and sparse-checkout settings belong in `git config --worktree`.
The default hooks directory is still shared unless `core.hooksPath` is scoped to
an independent directory. This permits tools such as prek to use distinct hooks
per checkout without collisions; Hive does not install hook shims itself.

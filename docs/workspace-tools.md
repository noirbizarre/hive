# Agent-facing workspace tools

Hive exposes a built-in workspace capability provider through its existing
native MCP-over-ACP aggregate, `hive-managed-mcp`. Agents discover it with
`server/discover` and `tools/list`, and invoke tools with `tools/call`.
The provider needs no configured MCP subprocess or additional credentials.

## Availability and ownership

The provider attaches automatically when a session has a registered workspace,
runs a node-local native ACPv2 agent, and that agent advertises `session.mcp.acp`.
Agents without this capability, ACPv1 agents and OpenCode REST adapters continue
to work without the automatic provider. Explicitly configured managed MCP servers
still require their existing transport capabilities.

An ordinary unregistered cwd does not become a workspace merely to expose tools.
Missing workspace association omits the provider. A workspace that becomes stale,
missing or unverifiable after attachment produces an error; Hive never substitutes
a directory inferred from the agent's arguments.

For remote sessions, the provider runs on the Hive node selected through remote
owner startup. It resolves that owner's durable session/workspace records and
executes through its workspace service. Initiating-node paths and plain remote
agent endpoint paths are not local filesystem authority. A foreign owner is
rejected before interpreting its path. Legacy records without an owner field
require independently verified local resource records.

## `hive.workspace.capabilities`

The tool's literal name is stable and can be called before `tools/list`.
Configured user tools keep their existing digest aliases, even when their original
names match a built-in. Unknown `hive.*` names cannot select user backends.

Input schema:

```json
{
  "type": "object",
  "properties": {},
  "additionalProperties": false
}
```

Omitted arguments mean `{}`. Explicit `null`, non-object arguments, paths,
session/workspace selectors and unknown fields are rejected.

Example `tools/call` parameters:

```json
{"name": "hive.workspace.capabilities", "arguments": {}}
```

The successful result has `resultType: "complete"`, `isError: false`, and
`structuredContent` containing this versioned object. Its `content` also contains
one text item with the same JSON for clients that consume textual results:

```json
{
  "schemaVersion": 1,
  "owner": "<owning-node-id>",
  "project": "demo",
  "workspace": "<workspace-id>",
  "repositoryBackend": "git",
  "provisioner": "git.worktree",
  "tools": [
    {
      "name": "hive.workspace.capabilities",
      "schemaVersion": 1,
      "effect": "read_only"
    },
    {"name": "hive.vcs.status", "schemaVersion": 1, "effect": "read_only"},
    {"name": "hive.vcs.diff", "schemaVersion": 1, "effect": "read_only"},
    {"name": "hive.vcs.commit", "schemaVersion": 1, "effect": "mutation"},
    {"name": "hive.vcs.log", "schemaVersion": 1, "effect": "read_only"},
    {"name": "hive.workspace.current", "schemaVersion": 1, "effect": "read_only"},
    {"name": "hive.workspace.info", "schemaVersion": 1, "effect": "read_only"},
    {"name": "hive.workspace.list", "schemaVersion": 1, "effect": "read_only"},
    {"name": "hive.workspace.create", "schemaVersion": 1, "effect": "mutation"},
    {"name": "hive.workspace.remove", "schemaVersion": 1, "effect": "mutation"},
    {"name": "hive.workspace.operation", "schemaVersion": 1, "effect": "read_only"}
  ]
}
```

`tools/list` supplies the full input and output JSON schemas. The canonical Rust
types and descriptors live in `hive_protocol::workspace_tools`. Required output
fields are `schemaVersion`, `owner`, `project`, `workspace`, `repositoryBackend`,
`provisioner` and `tools`. `repositoryBackend` is a string or null; the other
identities are strings. Each tool entry requires `name`, integer `schemaVersion`
and `effect` (`read_only` or `mutation`). Consumers must tolerate additive fields.

### Supported capabilities

Only implemented, callable tools appear in discovery and the capability result.
The same catalog controls invocation. Pinned backend/provisioner selection and
session scope are checked on invocation; a previous discovery result grants no
additional authority.

| Workspace implementation | `repositoryBackend` | `provisioner` | Callable built-ins |
| --- | --- | --- | --- |
| Native Git worktree | `git` | `git.worktree` | Capabilities, four VCS tools and six lifecycle tools |
| Independent Git clone | `git` | `git.clone` | Capabilities, four VCS tools and six lifecycle tools |
| Git with WorkTrunk | `git` | `worktrunk` | Capabilities, four VCS tools and six lifecycle tools |
| Native Jujutsu | `jj` | `jj.workspace` | Capabilities, four VCS tools and six lifecycle tools |
| Registered external directory | null | `external.directory` | Capabilities and lifecycle tools except create; removal still refuses external targets |

Provisioner lifecycle flags are not VCS-tool support. In particular, WorkTrunk
remains a Git workspace provisioner: VCS operations dispatch directly to its pinned
Git backend. Lifecycle creation inherits the pinned compatible provisioner instead
of selecting a new implementation from current configuration.

## Workspace lifecycle tools

The session's durable project attachment authorizes same-project workspaces on
the owning node. Explicit IDs select resources within that context; they never
grant cross-project or cross-node access. Inputs cannot contain arbitrary paths,
repository sources, policy names, backend IDs, shell commands, or discard flags.

| Tool | Input fields | Semantics |
| --- | --- | --- |
| `hive.workspace.current` | None (`{}`) | Inspect the attached workspace and fresh resource evidence. |
| `hive.workspace.info` | Optional `workspace` | Defaults to current; inspect an explicitly identified authorized sibling. |
| `hive.workspace.list` | Optional `limit`, `cursor` | Project-local workspace metadata sorted by workspace ID, default 20, maximum 100. Pass `nextCursor` as `cursor`; null ends pagination. |
| `hive.workspace.create` | Required `operation`; optional `base_ref`, `branch`, `revision` | Create an unattached managed sibling using inherited policy and provisioner. Git accepts `base_ref`/`branch`; JJ accepts `revision`. |
| `hive.workspace.remove` | Required `workspace`, `operation` | Preserve-only removal, subject to fresh existing cleanup checks. |
| `hive.workspace.operation` | Required `operation` | Inspect a committed durable operation snapshot without replaying effects. |

All inputs are strict objects. Omitted arguments mean `{}`; unknown fields and
explicit null values are rejected. Workspace and operation IDs use their canonical
`workspace-<UUID>` and `operation-<UUID>` forms. Base/branch/revision strings are
bounded to 4,096 UTF-8 bytes and validated using their native domain types.

### Sibling creation and safe removal

Creation inherits the current managed workspace's durable repository handle,
managed/ephemeral policy, retention, base intent and pinned compatible provisioner.
It does not inherit a concrete checkout path, native workspace identity, prepared
checkpoint or Git branch. An omitted Git branch generates a fresh task branch.
JJ retains its native revision semantics; Git branch intent is rejected for JJ.
Changing project defaults, repository sources or WorkTrunk configuration does not
replace these pinned choices. Bases resolve from the reusable owner-side repository;
creation does not copy the parent's uncommitted working files.

The new workspace does not replace the caller's session attachment. Absolute expiry
deadlines remain absolute; idle retention uses the new workspace's own lifecycle
baseline. Existing lifecycle and retention policies remain authoritative.

Removal always preserves changes and existing history-preservation behavior. Dirty
tracked files, untracked **and ignored** content, active or unknown use, external
ownership, unverified identity and uncertain evidence prevent deletion. The current
workspace remains protected by its session hold. A clean VCS status alone is not
cleanup authorization. Agent tools cannot authorize destructive discard or replay
an administrative discard operation. Ending a session never removes its workspace.

### Durable identity and operation inspection

Allocate an operation ID before each new mutation and retain it even if the response
is lost. Calls normally await completion and return that reference. Execution errors
also include the supplied `operation`. Repeating the same ID with changed intent
conflicts; an identical retry uses the original frozen request and checkpoints,
including cleanup's original target snapshot. Inspect first after interruption;
uncertain external effects and hooks require existing owner-side reconciliation.

```json
{
  "name": "hive.workspace.create",
  "arguments": {
    "operation": "operation-22222222-2222-4222-8222-222222222222",
    "base_ref": "main"
  }
}
```

Inspect that key with `hive.workspace.operation` and
`{"operation":"operation-22222222-2222-4222-8222-222222222222"}`. Inspection reads
committed state without waiting for the long-running resource lock or invoking a
backend. It never migrates records, resumes work or certifies deletion safety.
`persistedSnapshot: true` identifies this response: `state`, `failure` and `retry`
describe the associated workspace's **latest persisted lifecycle**, not a separate
live worker status or an immutable historical result for the queried operation.
`kind` and `effectsStarted` belong to the queried operation. Provisioning/cleaning
does not prove that a worker is alive, and an effects marker does not prove success.
A missing key means not recorded in that snapshot, not permission to allocate a
replacement key and repeat effects.

Workspace metadata contains `schemaVersion`, `owner`, `project`, `workspace`,
`state`, `ownership`, nullable `operation`, `failure`, `retry`, `repositoryBackend`
and `provisioner`. Current/info add `inspection.presence` and, for present/partial
resources, `inspection.safety` (`ownership_verified`, `cleanliness`, `usage`). List
returns `workspaces` and nullable `nextCursor`. Operation inspection adds `kind`,
`effectsStarted` and `persistedSnapshot`. Raw requests, prepared backend payloads,
holder session IDs, repository sources and internal paths are not returned.

Authority is checked after resource admission and before later mutation phases,
including deletion after preservation. Once a native effect has been dispatched,
Hive retains its checkpoint/result settlement across cancellation, close and caller
drop; these cannot turn a completed mutation into a safely retryable cancelled read.
Built-in work admitted during new/resume also counts as startup progress until its
resource job settles. After settlement, an otherwise silent agent remains subject
to the normal startup watchdog.

## VCS tools

All operations target the session's entire registered checkout, even when the agent
cwd is a subdirectory. Inputs cannot select another backend, root, repository or
node. The tool names include the literal `hive.` prefix.

| Tool | Input fields | Semantics |
| --- | --- | --- |
| `hive.vcs.status` | None (`{}`) | Structured native identity, tracked/index/physical changes, untracked and ignored files, conflicts and an observation token. |
| `hive.vcs.diff` | Optional `view`, `max_bytes` | Tracked changes only. `view` is `working_copy` (default), `staged` (Git only), or `unstaged`. |
| `hive.vcs.commit` | Required `message`, `inclusion`; optional `untracked`, `ignored`, `expected_state` | Record explicitly selected changes and return their native identity and resulting workspace state. |
| `hive.vcs.log` | Optional `limit` | Recent ancestry, newest first: default 20 entries, maximum 100. JJ includes its recorded working-copy change. |

Inputs reject unknown fields and explicit null argument objects. Commit messages
must contain non-whitespace text, no NUL, and at most 16 KiB of UTF-8. History
messages retain at most 16 KiB each and report `messageTruncated`. Native identities
always use full identifiers. Non-UTF-8 filenames produce an actionable unsupported
diagnostic rather than substituting a different filename.

### Explicit commit inclusion

There is no default inclusion mode:

| Backend and inclusion | Recorded content |
| --- | --- |
| Git `staged_only` | The existing index, including staged additions. Unstaged portions of partially staged files remain in the working tree. |
| Git `tracked_working_copy` | Native `git commit --all` semantics: tracked modifications/deletions and existing index additions. This can update the index. |
| JJ `tracked_working_copy` | Snapshot tracked physical files with automatic new-file tracking disabled, finalize the current change with the supplied description, then create a new working-copy change. |

JJ has no staging area, so `staged_only` is unsupported. Files already tracked by JJ
remain tracked even if native JJ previously auto-tracked them outside Hive.

Untracked and ignored files are excluded unless named explicitly in `untracked`
or `ignored`. These are separate arrays of literal checkout-relative **file** paths,
with at most 1,000 combined entries. They are not Git pathspecs or JJ filesets.
Directories, traversal, metadata paths, nested SCM paths and substituted directory
symlinks are rejected. Ignored-file inclusion requires the `ignored` array; it is
never implied by `untracked`. Explicit additions are unsupported with Git
`staged_only`. Newly included files must fit the 1 MiB explicit-addition bound;
JJ's native tracking restrictions still apply to ordinary untracked additions.

```json
{
  "name": "hive.vcs.commit",
  "arguments": {
    "message": "fix: handle missing workspace metadata",
    "inclusion": "tracked_working_copy",
    "untracked": ["src/new module.rs"],
    "expected_state": "<stateToken from status>"
  }
}
```

Git executes configured hooks and signing noninteractively, with the existing
120-second subprocess timeout. Hooks can reject or edit the message; results carry
the actual recorded message. JJ retains Hive's existing signing policy (`drop`).
Empty selections and unresolved conflicts are explicit errors. Git's in-progress
merge/rebase/cherry-pick/revert workflows are unsupported by the common commit tool.

### Native state and bounded diffs

Every successful VCS response contains `schemaVersion`, `operation`, `owner`,
`project`, `workspace`, `repository`, `repositoryBackend`, `outcome`, `state`,
`recorded`, `diff`, and `history`. Unused `recorded` is null; unused lists are empty.

`state` contains `identity`, nullable native Git `branch`, `stateToken`, `dirty`, `recordedChange`, `conflicts`,
`files`, and `complete`. Each file has `path`, nullable `originalPath` for Git
renames/copies, nullable `index` and `workingCopy`
statuses, and `untracked`, `ignored`, and `conflict` flags. Git uses native porcelain
status letters. JJ physical edits use `M`/`D`; `recordedChange` separately indicates
recorded content differing from its merged parents. Ignored files alone do not
make the workspace dirty.

An identity has `commitId`, nullable `changeId`, and nullable `operationId`. Git has
no synthetic change ID. JJ preserves both change and revision IDs; its operation ID
identifies the observed repository view. `recorded` is a change object containing
`identity`, `message`, and `messageTruncated`. After JJ commit, `recorded.identity`
identifies the finalized change and `state.identity` identifies its new successor.

Diff sections have `layer`, `text`, and `truncated`. Git `working_copy` compares
HEAD with physical tracked content, `staged` compares HEAD with the index, and
`unstaged` compares the index with physical content. External diff drivers and
text conversion are disabled. JJ inspections never snapshot files or enroll new
ones: `working_copy` returns separate `recorded_change` and `physical_edits`
sections, while `unstaged` returns physical edits only. These are layered
observations, not a single apply-ready patch. Binary edits have a binary marker.

The text budget is 64 KiB by default, configurable from 1 byte to 256 KiB across
all sections. Truncation preserves valid UTF-8 and drains native process output.
Non-UTF-8 textual diff bytes are replacement-decoded for display.
JJ physical-file display is bounded to 1 MiB per file; unavailable/oversized
content is explicitly omitted with `truncated: true`. Physical inspection hashes
files with a 128 MiB per-file bound and directory traversal is limited to 100,000
entries. Unsupported evidence fails explicitly. Status presents at most 1,000
file entries or 256 KiB of serialized file records, marking `complete: false`
when the presentation is partial. Commit rejects incomplete preflight state.

### Concurrent changes and interrupted mutations

`expected_state` is an optional optimistic precondition. Hive serializes its own
resource jobs and rechecks state and attachment authority before mutation. Native
SCM locks protect native transactions. External shells, editors and hooks remain
able to write; tokens and repeated observations are not atomic filesystem locks.
Detected stale state is a typed concurrency error.
Unexpected Git branch movement observed after a successful commit returns
`outcome: recorded_concurrent`, preserving the recorded identity for inspection.

Staging or JJ tracking/snapshotting may have effects before final commit. A failure
or timeout after dispatch produces `hive::vcs::uncertain`, directing the agent to
inspect status and history before retrying. Hive neither rolls back nor replays
these operations automatically. Cancellation before dispatch records no mutation;
after dispatch Hive retains actual settlement and returns a known result whenever
delivery is possible. A lost transport response may still hide a successful commit.
If commit succeeds but resulting state inspection fails, the result preserves its
recorded identity with `outcome: recorded_state_unavailable` and incomplete state;
that state is the last preflight observation, not a claimed post-commit snapshot.

## Authority, permissions and lifecycle

The provider captures the exact owner-side session control and workspace context.
It accepts calls during pending new/resume setup and after promotion to a live
session. Every operation rechecks shutdown, closing/ended status, control identity
and registered project. Resource operations additionally validate the durable
workspace hold, owner, pinned backend selection and fresh workspace evidence under
the existing resource admission gate. `hive.workspace.operation` instead validates
the durable attachment and project/node scope against one committed snapshot; it
does not take that gate or inspect filesystem/backend evidence. The provider rejects
stale authority after close, failed startup, worker exit or replacement during recovery.

Capability discovery, VCS status/diff/log and workspace current/info/list/operation
are read-only. An admitted agent may inspect and record its own workspace's changes
without a pilot approval interaction.
VCS tools cannot select another session/project/workspace or execute arbitrary
commands. Lifecycle tools allow explicit same-project sibling selectors only.
MCP annotations describe effects; they are not permission to mutate resources or
discard data. Commit requires explicit inclusion, fresh session authority and the
existing workspace safety contracts.

Listener authentication admits the connection under Hive's existing administrative
principal policy. The downstream session attachment supplies tool authority;
neither a caller-provided ID nor the UI pilot token supplies it. No new secret is
persisted in user MCP declarations.

Disconnect or pilot takeover alone does not revoke the session. Cancellation,
caller drop and close cannot manufacture completion of a running resource job.
Hive retains its settlement evidence until the actual service job finishes;
workspace release waits for that evidence. Ending a session never removes its
workspace automatically.

## Errors

Invalid arguments and unsupported tool names produce MCP JSON-RPC error `-32602`
with controlled `data.code`, `data.message` and `data.help`. A scoped tool execution
failure returns `isError: true` with a text item containing the same structured
diagnostic in text and `structuredContent`, not a successful tool result. Discovery-time scope or
authority failures use JSON-RPC `-32000` with diagnostic data. Transport cancellation
may instead surface through ACP's cancellation response.

| Stable diagnostic code | Meaning and action |
| --- | --- |
| `hive::workspace_tools::arguments` | Follow the advertised tool schema and only its documented selectors. |
| `hive::workspace_tools::unsupported` | Discover again and use an advertised tool name. |
| `hive::workspace_tools::scope` | Inspect and repair the owning session's registered workspace and backend. |
| `hive::workspace_tools::owner` | Reconnect to the Hive node owning the session/workspace. |
| `hive::workspace_tools::session` | Use the active attachment; closed/replaced providers cannot execute tools. |
| `hive::workspace_tools::cancelled` | Retry from an active session only if the read-only result is still needed. |
| `hive::workspace_tools::backend` | Inspect owner-side resource-store/backend diagnostics, repair, then retry. |
| `hive::vcs::arguments` | Check literal relative paths and the tool's input schema. |
| `hive::vcs::unsupported` | Use a supported operation/mode/view; inspect unsupported file or state evidence. |
| `hive::vcs::empty` | Select existing changes before recording. |
| `hive::vcs::conflict` | Resolve conflicts or finish the native SCM workflow first. |
| `hive::vcs::concurrent` | Inspect current status and refresh the state token. |
| `hive::vcs::backend` | Repair native SCM configuration/evidence on the owning node. |
| `hive::vcs::cancelled` | Authority ended before mutation; use an active session. |
| `hive::vcs::uncertain` | Inspect status and history before retrying; partial or complete effects are possible. |
| `hive::workspace::unsafe_cleanup` | Inspect structured `reason` (dirty, in use, unknown use/cleanliness, external/unverified ownership); preserve the resource and resolve the condition. |
| `hive::workspace::retry_conflict` | Restore the original operation ID and intent; never change a recorded cleanup's discard mode. |
| `hive::workspace::uncertain_operation` | Inspect the original operation and reconcile owner-side effects before retrying. |
| `hive::workspace::unknown` | The operation is not recorded in this committed snapshot; retain its original ID. |

Other lifecycle failures retain the existing domain diagnostic code, such as
`hive::workspace::request`, `hive::workspace::worktrunk` or `hive::jj::revision`.

Existing MCP facade limits still apply, including bounded concurrent operations
(`hive::mcp::busy`), response sizes and cursor validation. Errors do not expose
credentials, raw subprocess output or opaque stored backend payloads.

## Discovery and compatibility

The built-in catalog is the first tools-list page. If managed servers are selected,
its opaque continuation visits their pages without repeating built-ins. Other
list families keep their existing behavior. Discovery remains private with zero
TTL; callers should rediscover rather than treat a previous capability list as
permanent authorization.

The facade retains its MCP protocol version. Tool `schemaVersion: 1` is independent
of that version and Hive's package version. New optional fields and new separately
advertised tools are additive. Removing/renaming fields, changing their types or
semantics, or changing an existing tool's behavior incompatibly requires a new
versioned tool contract/name, with the old contract retained or an explicit
migration. Diagnostic codes are public stable identifiers.

User-configured MCP servers, inline declarations, direct HTTP forwarding and
agent-native shell workflows retain their existing behavior. Reserve the aggregate
declaration name `hive-managed-mcp` whenever a built-in or managed server attaches;
collisions fail atomically. Shell access remains available for general commands
and backend-specific workflows.

## Verification evidence

- `tests/mcp_routing.rs`: setup-time built-in-only and combined discovery/calls on
  HTTP and WebSocket, strict arguments, literal-name collisions with user tools,
  paging and transport compatibility.
- `src/workspaces/tests/mcp.rs`: session-hold and owner validation, stale scope,
  nested/duplicate roots, cancellation and actual resource settlement during close,
  caller drop, recovery and shutdown.
- `src/workspaces/tests/vcs.rs`: Git/JJ native state and identities, partial staging,
  explicit additions, ignored files, layered read-only JJ inspection, conflicts,
  hooks, mutation fences, bounded output and Git dispatch for WorkTrunk selection.
- `src/workspaces/tests/lifecycle_tools.rs`: inherited siblings and durable retries,
  project/owner authorization, preserve-only cleanup and session holds, committed
  snapshots while the resource gate is held, and revocation before checkout dispatch.
- Backend-specific gateway tests in `tests/support/workspaces.rs`:
  direct peer-to-owner Git/JJ/optional WorkTrunk capabilities, lifecycle and VCS operations
  during startup and restart, with independent native history checks. This
  exercises the selected owner's provider, rather than
  routing through an initiating Hive gateway.

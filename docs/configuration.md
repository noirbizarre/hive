# Configuration

Hive loads a root TOML file and any fragments it explicitly includes when the
daemon starts. Agent definitions describe how to reach agents. Named execution
profiles group agents, projects and workspace settings into working setups.
Neither starts agents until a session needs them.

## Remote Hive peers

The TUI creation picker (`o` to cycle owners) and browser **Hive instance** selector
use these registrations through the gateway's ACP discovery. Clients need only
the gateway connection; owner credentials remain on the gateway. See the
[TUI](guides/tui.md#remote-execution-owners) and
[browser](guides/web.md#remote-hive-instances) guides for owner-side directories,
managed profiles, and session attachment.

Register remote execution owners in the root file or included fragments:

```toml
[peers.workstation]
owner = "node-11111111-1111-4111-8111-111111111111"
endpoint = "https://workstation.example/acp"
name = "Workstation"
auth_token_env = "HIVE_WORKSTATION_TOKEN"
```

`owner` is the remote Hive's persistent advertised NodeId. Obtain it from that
owner's initialization response. `workstation` is only a local configuration
alias: clients use the NodeId on the wire. Duplicate aliases and owners are
rejected. Endpoints are exact HTTP(S) URLs without userinfo or fragments;
redirects are disabled. `name` and `auth_token_env` are optional. Token values
are read from the named variable when connecting and never appear in discovery.

Registration is inert and does not probe or launch agents. Clients enumerate
registrations with `_hive/peers/list`, discover owner-scoped agents/profiles with
`_hive/agents/list` or `_hive/profiles/list` and a `peer` parameter, then select
`_meta.hive.peer` plus `agent` or `profile` in ordinary `session/new`. Inventories
remain dynamic; no local alias per remote agent is needed. See the
[peer extension contract](acp-extensions.md#configured-peers-and-explicit-selection).

Reload publishes registrations atomically without network access. Existing
verified links retain their registration snapshot; new links use the current
endpoint and credentials and verify the advertised NodeId again. Removing a
registration prevents new connections without closing its remote sessions.
Changing an alias never changes session identity. There is no automatic peer
discovery or recursive routing through unconfigured owners.

Remote cwd, project, workspace and MCP inputs are evaluated by the owner. The
initiating Hive does not provision workspaces or validate remote paths against
its own filesystem. Intermediate hops accept absolute POSIX, Windows drive and
UNC wire paths; only the execution owner validates their local meaning and existence.
Retain the qualified `{owner, id}` session reference from
creation/listing, resolve it after reconnect, and explicitly resume. Cached
gateway aliases do not confer control or authority to restore the remote agent.

## MCP server declarations

Define MCP servers under `[mcp.servers.<id>]` in the root or included fragments,
then select them on an ACPv2-only agent profile. Portable server fields follow
[Bee's canonical TOML schema](https://github.com/noirbizarre/bee/blob/main/docs/mcp.md):
copy a Bee `[servers.<id>]` definition beneath Hive's `mcp` namespace without
changing its fields. Configuration loading validates and resolves references but
does not launch processes or connect to endpoints.

```toml
[mcp]
# Reserved for future global MCP settings; currently only servers is accepted.

[mcp.servers.files]
command = ["uvx", "example-mcp-server", "--read-only"]
cwd = "."

[mcp.servers.files.env]
SERVICE_TOKEN = { env = "SERVICE_TOKEN" }
MODE = "readonly"

[mcp.servers.docs]
url = "https://example.test/mcp"

[mcp.servers.docs.headers]
Authorization = { env = "DOCS_MCP_AUTHORIZATION" }

# Hive-owned lifecycle/security controls are separate from portable fields.
[mcp.servers.files.hive]
scope = "workspace"
idle_timeout_seconds = 60

[agents.coder]
command = "example-acp-agent"
mcp_servers = ["files", "docs"]
```

Hive manages stdio servers and exposes them through **one aggregated native
MCP-over-ACP service** named `hive-managed-mcp`. The agent must advertise
`session.mcp.acp`; HTTP selections require `session.mcp.http`. Managed servers
require a node-local agent. ACPv1 and OpenCode REST adapters do not support these
selectors. An unavailable managed server fails startup before downstream session
setup is sent. HTTP URL/headers go directly to the agent, which handles HTTP
availability; Hive does not start an HTTP process or proxy its connection.

Registered-workspace sessions also expose the built-in
[`hive.workspace.capabilities` and backend-supported VCS tools](workspace-tools.md)
through the same aggregate
when a local native ACPv2 agent advertises `session.mcp.acp`. No server selector is
required. This automatic provider is omitted for unsupported agents or unregistered
directories, preserving ordinary session startup. Reserve `hive-managed-mcp` for
this attachment even when no managed subprocess servers are selected.

Specify exactly one transport: a nonempty `command` string array selects managed
stdio, and `url` selects directly forwarded Streamable HTTP. The executable is
the first command element; subsequent elements are literal arguments. There is
no `transport` discriminator or separate `args`. The former declaration namespace
is not supported. Stdio accepts `cwd` and `env`; HTTP accepts `headers`. Unknown
fields, mixed transports and fields belonging to another transport are errors,
including empty tables. HTTP cannot declare Hive process controls.

`hive.scope` is `machine`, `project` or `workspace`, defaulting to `workspace`.
Workspace scope requires a verified registered concrete workspace. Omitted cwd
defaults to its root; relative cwd and relative executable paths resolve against
that root independently, not against the fragment directory or the configured
cwd. This anchoring is not a filesystem sandbox. Missing workspace authority is
an error, never an implicit switch to machine scope. Machine/project scope
requires an explicit absolute cwd and an absolute executable or bare PATH name.

Bare executable names are resolved using the captured **daemon** PATH (and
PATHEXT on Windows) before instance identity is computed. A server's explicit
`env.PATH` changes its child environment, not this initial lookup. Relative or
empty PATH entries are anchored to the verified workspace root for workspace
scope and skipped for other scopes. Paths are canonicalized and must exist at
admission; executable candidates must be executable files. There is no shell,
tilde, dotenv or command-substitution expansion.

Children inherit only explicitly configured environment entries plus this
platform baseline when present: `PATH`, `HOME`, `USERPROFILE`, `TMPDIR`, `TMP`,
`TEMP`, `SystemRoot`, `WINDIR`, `COMSPEC` and `PATHEXT`. Explicit entries override
the baseline. Other daemon/provider credentials are not automatically forwarded;
the agent profile's environment is independent. Child stderr is discarded.

Environment and header values are literal strings or whole-value
`{ env = "NAME" }` references. Export the complete header value, such as
`Bearer ...`; `${NAME}` inside a string is not interpolated. References read the
daemon environment, not the child overlay. Missing, empty or non-Unicode values
fail validation with value-free diagnostics. Resolved inputs are frozen at
configuration publication and pinned with session selections; they are not read
again at process launch. Reload uses the running daemon's environment, so changing
an invoking shell's exports cannot update it. Resolved credentials are neither
logged nor included in public configuration/discovery or durable session records.

HTTP endpoints require HTTPS, or HTTP on localhost/a loopback IP, without URL
userinfo or fragments. Header names are case-insensitive; duplicates, invalid
values and protocol/routing overrides are rejected. Reserved names include
`host`, `accept`, `content-type`, `content-length`, `connection`,
`transfer-encoding`, `mcp-session-id`, `mcp-protocol-version` and `last-event-id`.
Resolved headers are sent to the selected downstream agent, which owns HTTP
connections, redirect/credential handling, TLS/proxy settings, retries and consent.
Accepting Bee's portable syntax does not give Hive Bee's direct-client guarantees:
Hive uses trusted daemon configuration rather than Bee's untrusted project-file
discovery and launch approval. Hive does not discover `.agents/mcp.toml` or require
a separate MCP file. Backend initialization uses MCP `2025-11-25`, Hive client
identity and empty client capabilities/options/roots.

IDs use 1–128 ASCII letters, digits, `.`, `-` or `_`; `hive-managed-mcp` is reserved.
Definitions and profile selectors must be unique. A profile can select at most
32 servers. Inline native ACPv2 declarations retain their existing forwarding
and persistence behavior; configured declarations cannot collide with their names.
Bundled clients need no MCP composer to use profile selections.

Definitions compose through the ordinary `include` mechanism, with references
resolved after composition. For example:

```toml
# config.toml
include = ["conf.d/*.toml"]
```

```toml
# conf.d/work.toml
[mcp.servers.work-docs]
url = "https://work.example.test/mcp"

[agents.work-coder]
command = "example-acp-agent"
mcp_servers = ["work-docs"]

[profiles.work]
agents = ["work-coder"]
```

An agent can also select a server in another fragment, regardless of include
order. Server IDs must be unique across all files; duplicate definitions name
both sources and fail instead of overriding or extending one another. Empty
`[mcp]` tables are allowed in multiple files; unsupported global keys are errors
in every source. Named execution profiles select reusable agents; MCP selectors
belong on those agents.

### Explicit process sharing

Private session instances are the default. Scope alone never authorizes sharing.
To opt in, the daemon operator supplies a complete trusted sharing assertion:

```toml
[mcp.servers.files.hive.sharing]
mode = "shared"
principal = "local-operator"
partition = "trusted-team"
policy_generation = "policy-3"
external_generations = { credential_file = "rotation-12" }
```

All fields are required. An explicit empty `external_generations = {}` asserts
that no external credential source remains unaccounted for. Hive cannot verify
this assertion: advance external generations when credential files/helpers rotate
and policy generation when access policy changes. These are opaque revision IDs,
not secrets or secret hashes. Agent/client metadata cannot grant sharing authority.
The [MCP identity contract](adr/012-mcp-transport-scope-and-instance-identity.md)
also compares scope, node, executable, arguments, complete environment, cwd and
initialization inputs. Equal operator policy does not override those differences.

### Aggregated discovery and invocation

The agent-facing facade implements native MCP `2026-07-28` discovery, ping,
tools list/call, resources list/template-list/read and prompts list/get over the
legacy stdio backends. Discovery is fresh on request, with zero TTL and private
cache scope. Tasks, multi-round continuations, subscriptions, sampling, elicitation,
completion, logging configuration and push list-change notifications are unsupported.

Configured tools/prompts receive stable bounded `hive_t_…` / `hive_p_…` aliases mapped to
the exact original names, with display titles identifying backend and original title/name.
Discover names before invoking them. Resource identifiers
use `hive-mcp://v1-b<hex-definition-id>/<original-uri>`; preserve the entire URI
byte-for-byte. Templates keep their original expressions behind that literal prefix.
Hive also rewrites standard resource links/embedded resources in tool and prompt
content. Arbitrary tool arguments, structured JSON, schemas and text URLs remain
backend-native; Hive does not guess which application strings are resource IDs.

Built-in tools keep their documented literal names and appear on the first tool
page. Subsequent pagination visits one backend page at a time in definition-ID order. Cursors are
opaque, attachment-local, method-specific and expire after 60 seconds. Restarted
backend generations invalidate continuations; rediscover without a cursor. Bounded
maps, cursors and frame sizes can return controlled capacity errors rather than
silently returning incomplete catalogs. Backend failures are sanitized; published
calls are never replayed automatically. Cancellation does not imply that a tool
had no side effects, and cleanup waits for actual backend settlement.

Live sessions pin selected definitions and resolved values across reload. Saved sessions retain their
original selector IDs; daemon restart resolves those IDs against current definitions
and re-admits them. Missing definitions fail recovery. Legacy saved sessions do not
gain newly added profile selectors. Generated ACP IDs, effective environments and
sharing snapshots are not persisted.

### Trusted embedding APIs

`Broker::new` is fallible: inline/programmatic configuration uses the same trusted
MCP preparation boundary as file-backed startup and reload. Trusted Rust embedders
can use `Broker::bind_scoped_mcp` with a `StdioAdmission`
to resolve scope from registered session resources. Workspace scope defaults to
the canonical concrete root; machine/project scope requires an explicit launch
cwd. Embedders still supply the resolved executable, complete effective
environment, initialization and trusted security/credential evidence. Hive does
not search PATH or infer credential completeness.

`Broker::mcp_execution_node`, low-level `Broker::bind_mcp` with fully resolved
identities, and `Broker::release_mcp` are also available. The Broker owns the
shared stdio runtime and durable session leases; client disconnects do not release
them. Configuration-driven routing uses the same ownership fences. See the
[runtime contract](adr/012-mcp-transport-scope-and-instance-identity.md#shared-stdio-runtime)
for initialization, cancellation, restart and cleanup behavior.

Scoped admission reserves managed workspaces exposed by the executable, cwd and
roots, including additional resources beyond the session's primary workspace.
Reservations last through idle timeout and confirmed process termination. After
a daemon restart, unresolved reservations appear in workspace recovery reports;
missing sessions alone do not authorize releasing them. See the
[scoped admission contract](adr/012-mcp-transport-scope-and-instance-identity.md#scoped-broker-admission)
for registration and recovery requirements.

## File location and precedence

The file is selected in this order:

1. `--config /path/to/config.toml`.
2. `HIVE_CONFIG`.
3. `$XDG_CONFIG_HOME/hive/config.toml`.
4. `$HOME/.config/hive/config.toml` when `XDG_CONFIG_HOME` is unset.
5. `./hive/config.toml` when both variables are unset.

An absent default file produces an empty configuration. An absent explicitly
selected file is an error. Unknown fields are rejected rather than silently ignored.

Use the same file when checking and serving:

```sh
hive doctor --config /absolute/path/config.toml
hive daemon --config /absolute/path/config.toml
```

## Configuration fragments

Use the root `include` setting to group profiles into separate files:

```toml
# ~/.config/hive/config.toml
include = ["conf.d/*.toml"]
default_agent = "claude"
```

```toml
# ~/.config/hive/conf.d/claude.toml
[agents.claude]
command = "claude-agent-acp"
```

Each fragment can contain `[profiles.<id>]`, `[agents.<id>]`, `[mcp.servers.<id>]`,
`[projects.<id>]`, `[workspaces.<id>]`
and global `[worktrunk]` tables with the
same fields as the root. `include`, `default_agent` and `default_profile` are
root-only settings. Defaults can reference declarations in any fragment.

Include entries are file paths or shell-style globs (`*`, `?`, character classes,
and `**` for recursive matching). Relative paths resolve beside the selected root
file, including when using `--config` or `HIVE_CONFIG`; absolute paths also work.
Environment variables and `~` are not expanded. A glob does not match hidden files
unless its pattern explicitly starts that component with a dot.

Hive reads root profiles first, then include entries in list order, sorting each
glob's matches by path. A glob matching nothing is allowed, including an absent
or empty `conf.d`. An explicitly named missing file, invalid pattern, unreadable
file, or directory selected as a fragment is an error. Glob traversal errors
reported by the filesystem matcher also fail loading.

Agent IDs must be unique across all files: agent definitions cannot override or
extend one another. Named execution profiles compose using the rules below.
Repeated files, overlapping includes, symlink aliases, and including
the root itself are rejected. There is no automatic `conf.d` discovery; without
`include`, only the root is loaded.

## Reloading changes

After editing, run `hive daemon reload`, or send SIGHUP to the daemon on Unix.
Hive rereads the original path, including files replaced atomically by an editor,
and re-expands its includes so fragment additions, removals, and edits are picked up.
It validates the entire candidate before publishing it. Invalid or unreadable
files leave the previous configuration active; deleting even an implicit default
file causes reload to fail rather than clear the registry.

New defaults and profiles apply to new sessions. Existing sessions keep their
original agents, connections, credentials, and pricing, even when their profile
is removed. Open browser and TUI clients refresh their provider lists automatically.
Reload does not retry recovery of offline sessions.

An OpenCode profile ID remains associated with its existing sessions, including
saved offline sessions. Changing its server connection identity or adapter kind
is rejected with `hive::reload::profile_identity`; use a new profile ID for the
new server. Removing and re-adding the old ID does not erase that association.
Connection identity conservatively includes endpoint, headers, authentication
username, and password environment-variable name, so routing/authentication edits
can also require a new ID. Secrets are not stored as plaintext in the identity.
Older saved sessions establish their initial identity from the startup profile;
that pre-upgrade identity cannot be independently verified. If the startup profile
is absent, an unknown legacy binding reserves its ID; use a new ID rather than
reusing it for another server.

Reload uses the daemon's environment, including `HIVE_DEFAULT_AGENT`; exporting
variables in the calling shell does not update a running process. The original
file path is retained even if the reload caller sets another `HIVE_CONFIG`.
Listen address, authentication, allowed browser origins, state/runtime locations,
and tracing settings require restart.

## Top-level settings

| Setting | Type | Default | Meaning |
| --- | --- | --- | --- |
| `include` | Array of strings | `[]` | Root-only paths or globs for configuration fragments |
| `default_agent` | String | Unset | Agent ID for session creation without an explicit agent |
| `default_profile` | String | Unset | Root-only named execution profile for new sessions |
| `profiles` | Table keyed by stable IDs | Empty | Named execution setups, composed across fragments |
| `agents` | Table of profiles | Empty | Available downstream agents |
| `mcp` | MCP settings table | Empty | Reserved global namespace; `mcp.servers` holds managed stdio or forwarded HTTP definitions |
| `projects` | Table keyed by project IDs | Empty | Portable configured definitions |
| `workspaces` | Table keyed by policy names | Empty | Workspace policy templates |
| `worktrunk` | Global integration table | Auto-detect | Optional WorkTrunk lifecycle selection |

`HIVE_DEFAULT_AGENT` overrides `default_agent`. The selected ID must exist.
A daemon can start without a default, but clients must then select an agent or
an execution profile with an unambiguous agent choice.

Profile IDs contain only ASCII letters, digits, `-`, or `_`, and cannot be empty.
The ID in `[agents.opencode]` is `opencode`. Its optional `name` is only a display
label and is not a selector.

## Named execution profiles

Agent definitions under `[agents]` remain compatible with existing configurations.
Without a selected execution profile, existing agent selection and workspace
behavior apply. A named profile references those agents and configured projects:

```toml
# config.toml
default_profile = "personal"
include = ["conf.d/*.toml"]
```

```toml
# conf.d/personal.toml
[profiles.personal]
name = "Personal"
agents = ["bee"]
projects = ["hive", "bee"]
default_agent = "bee"
default_project = "hive"
```

```toml
# conf.d/workspace-settings.toml
[profiles.personal.workspaces.hive]
policy = "worktree"
base_ref = "main"

[profiles.personal.workspaces.bee]
template = "task"
```

Declare `agents.bee`, both projects, and `workspaces.task` elsewhere in the composed
configuration. Profile references resolve after all fragments have been composed.
They reference configured projects, not mutable managed-project database entries.

`name`, `default_agent`, and `default_project` are optional. `agents` must contain
at least one configured agent. `projects` can be empty for cwd-only setups.
Defaults must belong to their respective member lists. A `workspaces` key must
belong to `projects`. Each workspace entry uses either a `template` reference to
a top-level workspace template or an inline `policy` with that policy's supported
settings. Mixing the forms, or supplying inline overrides beside `template`, is
an error. Policies must be compatible with the referenced project's SCM.

### Composition rules

| Profile field | Merge behavior across root and fragments |
| --- | --- |
| `agents`, `projects` | Sorted set union; duplicates and empty contributions have no effect |
| `name`, `default_agent`, `default_project` | Missing contributes nothing; equal values agree; unequal values conflict |
| `workspaces` | Merge by project ID, then by explicitly supplied field using the same scalar rule |

An inline workspace's `policy` and its settings may be supplied by different
fragments. Defaults apply only after composition. Conflicts report the profile,
field path and both source files, without printing their values. Existing duplicate
rules for agents, projects and top-level workspace templates still apply.

Profile IDs use nonempty ASCII letters, digits, `-` or `_`. Profile and agent IDs
are separate namespaces and may coincide. Moving or renaming a fragment never
changes a profile's ID. Files are organizational, not profile owners.

### Selection and overrides

Select a profile with `hive tui --profile personal`, the web **Execution profile**
selector, or `_meta.hive.profile` on `session/new`. Omission uses `default_profile`,
if configured. Selection is per request, not a daemon-wide active setup.

| Choice | Precedence within a selected execution profile |
| --- | --- |
| Agent | Explicit agent → profile `default_agent` → global `default_agent` if a member → sole member |
| Project for workspace creation | Explicit project → profile `default_project` → agent's project if a member → sole member |
| Workspace policy | Explicit named policy → profile's per-project policy/template → agent's workspace template |
| Base/revision/branch | Explicit request selectors override policy defaults using the normal workspace rules |

Explicit agents/projects outside membership fail. Legacy defaults outside
membership are skipped. Ambiguous choices report how to select explicitly or set
a default; list order never chooses an agent or project. A project is required
only when creating a workspace. Membership alone does not change cwd or provision
a checkout. An applicable workspace policy triggers automatic provisioning.

An explicit existing directory bypasses managed provisioning. Attaching a local
workspace requires its project to belong to the selected profile. Membership is
working-setup availability, not a filesystem isolation boundary. Native adoption
checks agent membership but does not provision a workspace. Resume/recovery keeps
the recorded concrete agent and workspace, even after profile edits or removal.
WorkTrunk settings remain global. Persisted agent-profile IDs retain their existing
meaning; no migration is required.

## Logical projects

Project IDs are portable lowercase slugs such as `hive` or `my-project`.
Registration generates a slug from the display name and appends `-2`, `-3`, and so
on if it is already taken by a configured or managed definition. Non-ASCII and
non-alphanumeric characters become separators; names without ASCII letters or
digits fall back to `project`. Explicit IDs use lowercase ASCII letters, digits,
and single hyphens, without leading or trailing hyphens. Existing UUID-based
project IDs remain valid. Preserve the chosen ID across renames, configuration-file
moves, and checkout relocations; changing a display name never changes the ID.

```toml
[projects.hive]
name = "Hive"
repository_source = "https://github.com/noirbizarre/hive.git"
default_ref = "main"
preparation = [["mise", "install"]]

[agents.coder]
command = "some-acp-agent"
project = "hive"
```

`name` is required and nonblank. `scm` defaults to `"git"`; select `"jj"` for
Jujutsu. Use `default_ref` for Git or `default_revision` for JJ; mixing them is an
error. Other project fields are optional. Remotes accept
HTTPS, SSH URLs, and SCP-style SSH (`git@host:team/repo.git`); filesystem sources,
other schemes, passwords, HTTP userinfo, query strings, and fragments are rejected.
Configure Git/SSH credentials on each execution node. Preparation stores argv
arrays with nonblank executables and no NUL characters; arguments are ordinary
metadata, not a place for secrets. Registration does not contact Git, clone a
repository, resolve refs, or execute preparation. Omitted `default_ref` leaves
default-ref selection to the fetched remote default branch.

A local-only project can omit `repository_source`. Existing cwd-based sessions
and the core `existing` directory policy remain available. An agent's optional
`project` alone declares and validates intent without changing cwd. Adding an
explicit `workspace` policy activates provisioning before downstream agent startup.

Project IDs must be unique across all selected files. References resolve after
complete composition, including declarations in later fragments. Profile references
must target **configured** projects, so doctor and configuration validation do not
depend on mutable SQLite state.

Configured definitions are read-only through the operational API and CLI: edit
TOML and reload. Separately, `hive project register` creates a durable, managed
definition in SQLite. The effective registry lists both origins; collisions fail
startup or reload rather than silently overriding either definition. To transfer
a managed ID into TOML, remove its managed definition before loading the declaration.

Node-local directory overrides are stored separately for each project/node ID.
Different nodes can use different paths for the same project. Overrides are
preferences, not clone/cache records or runtime workspaces; paths need not exist
at registration time. Removing a definition never deletes directories, clones,
workspaces, or overrides. See [project commands](reference/cli.md#projects).

## Workspace policies

Policies are independent named declarations, not runtime workspace IDs:

```toml
[workspaces.task]
policy = "worktree"
base_ref = "main" # optional: request, policy, project default, then remote HEAD

[workspaces.independent]
policy = "clone"

[workspaces.local]
policy = "existing"
directory = "/home/user/work/project"

[agents.coder]
command = "some-acp-agent"
project = "hive"
workspace = "task"

[worktrunk]
enabled = false
```

Project, workspace and agent declarations may live in separate included files.
All references resolve after composition; duplicates are errors, including multiple
global `[worktrunk]` declarations. Files never become profiles or contexts.
An `existing` policy may omit its directory to use the project's override for the
local node (whose stable identity is returned by `hive repository list`).

Worktree and clone policies create a distinct `hive/<workspace-id>` branch unless
the caller supplies a fresh branch. Each new Git-backed provision fetches first;
fetch failure prevents startup rather than silently using stale refs. Existing
workspaces are never reset by fetch. Identical operation retries retain their
original resolved base and backend. Changing a project source creates a new cache
generation rather than repointing existing workspaces.

`ephemeral` policies specify `backing = "worktree"`, `"clone"` or `"jj"` and a finite
`retention` (for example `retention = { kind = "until", expires_at = 1800000000 }`).
This records lifetime intent; automatic expiration execution is separate work.
Session shutdown never implies removal.

An explicit `--cwd` / Hive `existing` launch selector bypasses a profile's policy.
Without a policy or selector, ordinary ACP cwd behavior is preserved, including
downstream-only paths when using a remote agent. Explicit existing-directory
selections are node-local. A workspace-ID selector attaches to that ready resource
instead of provisioning.
Native adoption and session resume cannot request a replacement workspace.

### Native Jujutsu workspaces

Install JJ 0.46 or newer and Git on the owning Hive node, then opt in explicitly:

```toml
[projects.jj-project]
name = "JJ project"
scm = "jj"
repository_source = "https://example.com/team/project.git"
default_revision = "trunk()"

[workspaces.jj-task]
policy = "jj"
# revision = "main@origin" # optional policy override

[agents.jj-coder]
command = "some-acp-agent"
project = "jj-project"
workspace = "jj-task"
```

JJ uses a non-colocated repository acquired with `jj git clone` and isolated native
`jj workspace add` working copies. It creates neither Git branches nor Git worktrees.
The global `[worktrunk]` setting is ignored for JJ, including explicit enablement.
Git policies and JJ policies must match the selected project SCM.

Base selection is request `revision`, policy `revision`, project `default_revision`,
then JJ's `trunk()`. Expressions can use JJ change IDs, commit IDs, bookmarks and
revsets; they must resolve to exactly one conflict-free commit. Hive pins that
commit before creating a new working-copy change on top of it. Identical operation
retries keep that resolution. JJ ephemeral policies use `backing = "jj"` and the
project/request revision rather than Git's `base_ref`.

Initial remotes use JJ's explicit Git interoperability over the same HTTPS/SSH
formats as Git projects. Filesystem sources and external/colocated repositories
are not acquired through this policy. JJ/Git credential helpers, URL rewrites and
SSH credentials remain node-local. The Git CLI is used to inspect the original
stored remote locator through `jj git root`; provisioning uses native JJ commands.
Hive disables signing for its unattended JJ operations.

Inspection does not snapshot the working copy. A nonempty working-copy change is
dirty even after JJ has snapshotted it; filesystem edits, untracked and ignored
files also block normal cleanup. Unsupported or incomplete evidence blocks cleanup,
including discard. Explicit discard permits deleting dirty data only when ownership
and idle usage are verified. Cleanup retains the current change/history under a
`hive-retained-<workspace-id>` bookmark in the shared repository, removes the actual
workspace root, and forgets its native registration. Review retained bookmarks
before deleting them manually. Other workspaces and repository history are retained.

All operations run on the owning daemon. Negotiated peers can transfer portable
project definitions and request [owner-side provisioning](acp-extensions.md#provisioning-on-the-selected-owner).
The selected node resolves its own workspace policies, credentials and overrides.

### Global WorkTrunk integration

`enabled = true` requires compatible `wt`; `false` bypasses it without probing.
An absent or empty section auto-detects a usable installation, otherwise selecting
native Git. This choice is global, never per-profile. Selected WorkTrunk owns
worktree creation/removal and directory templates: Hive supplies no target path
and persists the actual returned directory. Failures never fall back mid-operation.

This setting selects the Git worktree provisioner, not the repository/SCM backend.
Native Git and WorkTrunk share the same acquired Git cache; independent clone and
existing-directory policies keep their own provisioning behavior. Each workspace
and operation pins the repository backend, storage handle and provisioner before
effects. Reloading this setting affects new selection only: retry, inspection,
reconciliation and cleanup keep their recorded implementations.

Hooks remain enabled and require prior WorkTrunk approval. For a new bare cache,
Hive reads project configuration from the pinned base commit and supplies that
snapshot to WorkTrunk so branch-specific hooks are not silently skipped. Inspect
the operation for its `worktrunkConfig` snapshot path, review it, then use
`WORKTRUNK_PROJECT_CONFIG_PATH=<snapshot> wt -C <bare-repository> config approvals add`
before retrying the original operation. Hive never passes `--yes`. Blocking hook
failures prevent readiness; background hooks retain WorkTrunk's asynchronous
semantics. Interrupted hooks require inspection/manual reconciliation, not blind
replay.

Hive also publishes its own ACP activity through `wt config state marker` for
verified local Git worktrees, including attached/adopted sessions and worktrees
created outside Hive. Marker support requires `get` with JSON output and explicit
branch selection for `get`, `set`, and `clear`. Explicit enablement reports missing
capabilities rather than silently selecting a fallback. Non-Git repositories,
detached checkouts, foreign-node workspaces, and unverified remote-provider paths
do not publish markers on the gateway. Remote execution needs a node-local Hive
owner; this integration does not infer ownership from a matching cwd string.

| Activity evidence | Marker prefix |
| --- | --- |
| Running foreground work | `🤖 Hive` |
| Needs input, including a pending permission/question | `💬 Hive` |
| Explicit successful end of turn | `✅ Hive` |
| Confirmed cancellation | `⏹ Hive` |
| Failure or refusal | `❌ Hive` |

Markers include a unique ownership suffix. Needs-input takes precedence over
running across sessions sharing a branch; otherwise the latest known terminal
outcome remains visible while its session is open. Unknown execution is not
success and suppresses terminal-only results. Closing or losing the last session
withdraws Hive's marker. Client disconnection does not close a session.

The activity ledger survives restart and preserves foreign marker values observed
before an update or clear. See [activity recovery](operations.md#worktrunk-activity-recovery)
for reconciliation and shared-marker limitations. Disabling the integration stops
marker probes and writes, deferring any pending marker cleanup until it is enabled
again. Previously pinned workspace lifecycle operations retain their provisioner.

### Flat bare layout and independent hook settings

New worktree repositories prefer this layout:

```text
<project-root>/
  .git/                    # bare shared repository
  main/                    # example default-branch checkout
  workspace-.../           # independent native task checkout
```

Native task directories are immediate children even when branch names contain
slashes. Existing layouts are retained; WorkTrunk's selected paths take precedence.
Independent clone policies use separate managed directories.

New flat bare repositories enable `extensions.worktreeConfig`. Following
[Git's configuration rules](https://git-scm.com/docs/git-worktree#_configuration_file),
`core.bare=true` resides in the bare repository's `config.worktree`, not shared
config. Linked checkouts can therefore have independent settings. The extension
does not by itself isolate Git's default shared hooks directory. Inside each
checkout, give hook tools an independent directory:

```sh
git config --worktree core.hooksPath "$(git rev-parse --absolute-git-dir)/hooks"
prek install
```

Current prek honors worktree-local `core.hooksPath`. If installing hooks from
WorkTrunk lifecycle hooks, configure that path before `prek install` there too.
Use `git config --worktree`, rather than `--local`, for branch-specific hook and
sparse-checkout settings. Hive does not install hook shims automatically.

Worktrees isolate checkouts, **not processes or filesystem access**. They are not
security sandboxes.

## Profile fields

| Field | Type | Default | Applies to |
| --- | --- | --- | --- |
| `name` | String | Profile ID for display | All profiles |
| `project` | Project ID | Unset | Optional configured project reference, validated after composition |
| `workspace` | Policy name | Unset | Provision this policy before startup; requires `project` |
| `mcp_servers` | Array of server IDs | `[]` | Configured managed stdio/forwarded HTTP selections; ACPv2-only |
| `kind` | `"acp"` or `"opencode-server"` | `"acp"` | All profiles |
| `command` | String | Unset | ACP stdio executable |
| `args` | Array of strings | `[]` | ACP stdio arguments |
| `env` | String-to-string table | `{}` | ACP stdio environment overrides |
| `endpoint` | URL string | Unset | ACP network or OpenCode server |
| `headers` | String-to-string table | `{}` | Endpoint request headers |
| `versions` | Array of `"v1"` / `"v2"` | `["v2"]` | ACP version selection |
| `username` | String | `"opencode"` when using password auth | OpenCode server |
| `password_env` | String | Unset | Name of OpenCode password environment variable |
| `pricing` | Table | No price mapping | Usage estimates |

For ACP, choose either `command` or `endpoint`. A command profile cannot have
headers. An endpoint profile cannot have `command`, `args`, or `env`.
`username` and `password_env` are rejected on ACP profiles.

`versions` cannot be empty. Hive prefers v2 when both versions are declared.
An initialization failure does not trigger a v1 fallback. The OpenCode server
adapter uses REST/SSE rather than selecting an ACP wire version.

## ACP over stdio

```toml
default_agent = "opencode"

[agents.opencode]
name = "OpenCode ACP"
command = "opencode"
args = ["acp"]
versions = ["v1"]
env = { EXAMPLE_SETTING = "value" }
```

`command` is an executable, not a shell command line. Put each argument in its
own array element. Use an absolute executable path when launch location matters.
The process inherits the daemon environment with profile overrides applied.

Each session gets its own downstream connection and, for stdio, its own process.
Check the agent's actual entrypoint and supported version before selecting it.

## ACP over a network transport

```toml
[agents.network-agent]
endpoint = "http://127.0.0.1:9000"
versions = ["v2"]
headers = { Authorization = "Bearer replace-with-your-token" }
```

ACP endpoints accept `http://`, `https://`, `ws://`, and `wss://`. The endpoint
must serve ACP, not an unrelated REST API. See the [transport matrix](transports.md).

Header strings are literal. Hive does not expand `${TOKEN}` in configuration
strings. Keep configuration containing credentials private and omit it from
public bug reports.

## OpenCode server

```toml
[agents.opencode-server]
kind = "opencode-server"
endpoint = "http://127.0.0.1:4096"
username = "opencode"
password_env = "OPENCODE_SERVER_PASSWORD"
```

Start and authenticate the server separately. Hive does not launch it.
`password_env` names a variable in the **daemon's** environment, not a secret
value. If configured, that variable must be set and nonempty when connecting.

This profile cannot contain `command`, `args`, or `env`. Its endpoint must be
HTTP(S), must not embed credentials, and may use plain HTTP only on loopback.
These stricter endpoint checks are specific to the OpenCode adapter. They are
not a promise that all ACP endpoints enforce the same restrictions.

See [OpenCode integration](agents/opencode.md) for native-session adoption.

## Pricing

Usage tracking works without pricing. Estimates require the exact billed
provider and model identifiers from [models.dev](https://models.dev/).

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `provider` | String | Unset | Exact models.dev provider ID |
| `model` | String | Unset | Fixed catalog model ID |
| `models` | String-to-string table | `{}` | Agent selector ID → catalog model ID |
| `cached_input_included` | Boolean | `false` | Input token count includes both cache categories |

For a fixed model:

```toml
[agents.opencode.pricing]
provider = "anthropic"
model = "claude-sonnet-4-5"
cached_input_included = true
```

Use this only if those are the actual provider/model and cache semantics of
your agent. For selectable models, replace `model` with a mapping:

```toml
[agents.opencode.pricing]
provider = "anthropic"
models = { "agent-model-selector" = "claude-sonnet-4-5" }
```

A price mapping needs a provider and either a fixed model or selector mappings.
Hive does not infer billing identity from the profile name. Unknown mappings,
missing token categories, or catalog failures produce unknown estimates rather
than fabricated zero costs. If cache counts are reported without confirmed
inclusive-input semantics, ordinary input cost can remain unknown.

Provider-reported cumulative costs and estimates are distinct. Read the
[usage contract](compatibility.md#token-usage-and-costs) for aggregation rules.

## Runtime locations

| Purpose | Preferred location | Fallback |
| --- | --- | --- |
| Configuration | `$XDG_CONFIG_HOME/hive/config.toml` | `$HOME/.config/hive/config.toml` |
| Instance lock | `$XDG_RUNTIME_DIR/hive/daemon.lock` | `$HOME/.local/state/hive/daemon.lock` |
| SQLite catalog | `$XDG_STATE_HOME/hive/sessions.sqlite3` | `$HOME/.local/state/hive/sessions.sqlite3` |

The lock fallback does not use `XDG_STATE_HOME`. Lock and database lookup fail
when neither their XDG variable nor `HOME` is available.

Hive creates private runtime/state directories. On Unix, existing directories
or files with unsafe permissions can prevent startup. See
[operations](operations.md#storage-and-backups) before copying or changing state.

## Listener and client settings

Listener addresses and client connection options are CLI/environment settings,
not TOML fields. The daemon defaults to `127.0.0.1:8765`; non-loopback listeners
require explicit `--allow-remote`. Authentication is optional and independent of
this opt-in. For a different local port:

```sh
hive daemon --listen 127.0.0.1:9000
hive daemon status --url http://127.0.0.1:9000
hive tui --address 127.0.0.1:9000
hive web --url http://127.0.0.1:9000
```

The daemon/doctor use `HIVE_LISTEN`. Web and status support `HIVE_URL`
and fall back to `HIVE_LISTEN`. The TUI uses its own `--address` and does not
read either variable. See the [CLI reference](reference/cli.md) for precedence.

`--auth-token-env NAME` / `HIVE_AUTH_TOKEN_ENV` names the variable holding the
optional shared token. The same selector is used by native clients, including
project/workspace commands and TUI catalog/ACP connections. Set it in each
process's environment. `--allowed-origin` / `HIVE_ALLOWED_ORIGINS` configures exact
browser origins; `--allow-remote` / `HIVE_ALLOW_REMOTE` controls non-loopback binding.
These are startup-only settings, not root or fragment TOML fields. See
[private-network deployment](operations.md#private-network-access).

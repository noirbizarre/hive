# Configuration

Hive loads a root TOML file and any fragments it explicitly includes when the
daemon starts. Profiles describe how to reach agents. They do not start agents
until a session needs them.

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

Each fragment can contain `[agents.<id>]`, `[projects.<id>]`, `[workspaces.<id>]`
and global `[worktrunk]` tables with the
same fields as the root. `include` and `default_agent` are root-only settings. A default can select a
profile declared in a fragment. The root can also declare profiles directly.

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

Agent IDs must be unique across all files: profiles cannot override or extend
one another. Repeated files, overlapping includes, symlink aliases, and including
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
Listen address, state/runtime locations, and tracing settings require restart.

## Top-level settings

| Setting | Type | Default | Meaning |
| --- | --- | --- | --- |
| `include` | Array of strings | `[]` | Root-only paths or globs for configuration fragments |
| `default_agent` | String | Unset | Profile ID for session creation without a selector |
| `agents` | Table of profiles | Empty | Available downstream agents |
| `projects` | Table keyed by project IDs | Empty | Portable configured definitions |
| `workspaces` | Table keyed by policy names | Empty | Workspace policy templates |
| `worktrunk` | Global integration table | Auto-detect | Optional WorkTrunk lifecycle selection |

`HIVE_DEFAULT_AGENT` overrides `default_agent`. The selected ID must exist.
A daemon can start without a default, but clients must then select a profile.

Profile IDs contain only ASCII letters, digits, `-`, or `_`, and cannot be empty.
The ID in `[agents.opencode]` is `opencode`. Its optional `name` is only a display
label and is not a selector.

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

`name` is required and nonblank. Other project fields are optional. Remotes accept
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

`ephemeral` policies specify `backing = "worktree"` or `"clone"` and a finite
`retention` (for example `retention = { kind = "until", expires_at = 1800000000 }`).
This records lifetime intent; automatic expiration execution is separate work.
Session shutdown never implies removal.

An explicit `--cwd` / Hive `existing` launch selector bypasses a profile's policy.
Without a policy or selector, ordinary ACP cwd behavior is preserved, including
downstream-only paths when using a remote agent. Explicit existing-directory
selections are node-local. A workspace-ID selector attaches to that ready resource
instead of provisioning.
Native adoption and session resume cannot request a replacement workspace.

### Global WorkTrunk integration

`enabled = true` requires compatible `wt`; `false` bypasses it without probing.
An absent or empty section auto-detects a usable installation, otherwise selecting
native Git. This choice is global, never per-profile. Selected WorkTrunk owns
worktree creation/removal and directory templates: Hive supplies no target path
and persists the actual returned directory. Failures never fall back mid-operation.

Hooks remain enabled and require prior WorkTrunk approval. For a new bare cache,
Hive reads project configuration from the pinned base commit and supplies that
snapshot to WorkTrunk so branch-specific hooks are not silently skipped. Inspect
the operation for its `worktrunkConfig` snapshot path, review it, then use
`WORKTRUNK_PROJECT_CONFIG_PATH=<snapshot> wt -C <bare-repository> config approvals add`
before retrying the original operation. Hive never passes `--yes`. Blocking hook
failures prevent readiness; background hooks retain WorkTrunk's asynchronous
semantics. Interrupted hooks require inspection/manual reconciliation, not blind
replay. Activity markers are tracked separately in issue #118.

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
not TOML fields. The daemon defaults to `127.0.0.1:8765` and rejects non-loopback
listeners. For a different port:

```sh
hive daemon --listen 127.0.0.1:9000
hive daemon status --url http://127.0.0.1:9000
hive tui --address 127.0.0.1:9000
hive web --url http://127.0.0.1:9000
```

The daemon/doctor use `HIVE_LISTEN`. Web and status support `HIVE_URL`
and fall back to `HIVE_LISTEN`. The TUI uses its own `--address` and does not
read either variable. See the [CLI reference](reference/cli.md) for precedence.

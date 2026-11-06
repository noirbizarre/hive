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
    }
  ]
}
```

`tools/list` supplies the full input and output JSON schemas. The canonical Rust
types and descriptors live in `hive_protocol::workspace_tools`. Required output
fields are `schemaVersion`, `owner`, `project`, `workspace`, `repositoryBackend`,
`provisioner` and `tools`. `repositoryBackend` is a string or null; the other
identities are strings. Each tool entry requires `name`, integer `schemaVersion`
and `effect` (currently `read_only`). Consumers must tolerate additive fields.

### Supported capabilities

Only implemented, callable tools appear in discovery and the capability result.
The same catalog controls invocation. Pinned backend/provisioner selection and
session scope are checked on invocation; a previous discovery result grants no
additional authority.

| Workspace implementation | `repositoryBackend` | `provisioner` | Callable built-ins |
| --- | --- | --- | --- |
| Native Git worktree | `git` | `git.worktree` | `hive.workspace.capabilities` |
| Independent Git clone | `git` | `git.clone` | `hive.workspace.capabilities` |
| Git with WorkTrunk | `git` | `worktrunk` | `hive.workspace.capabilities` |
| Native Jujutsu | `jj` | `jj.workspace` | `hive.workspace.capabilities` |
| Registered external directory | null | `external.directory` | `hive.workspace.capabilities` |

Provisioner lifecycle flags are not VCS-tool support. In particular, WorkTrunk
remains a Git workspace provisioner. VCS status/diff/commit/log tools belong to
[#142](https://github.com/noirbizarre/hive/issues/142); workspace current/info,
list/create/remove belong to [#143](https://github.com/noirbizarre/hive/issues/143).
Those tools are not advertised until their handlers are implemented.

## Authority, permissions and lifecycle

The provider captures the exact owner-side session control and workspace context.
It accepts calls during pending new/resume setup and after promotion to a live
session. Every operation rechecks shutdown, closing/ended status, control identity,
registered project, durable workspace hold, owner, pinned backend selection and
workspace evidence under the existing resource admission gate. It rejects a stale
provider after close, failed startup, worker exit or replacement during recovery.

The capability tool is read-only, non-destructive and idempotent. An admitted agent
may inspect its own workspace's capabilities without a pilot approval interaction.
It cannot select another session/project/workspace or execute arbitrary commands.
MCP annotations describe effects; they are not permission to mutate resources or
discard data. Future mutating tools must enforce their own explicit permissions
and the existing workspace safety contracts.

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
diagnostic; it has no successful `structuredContent`. Discovery-time scope or
authority failures use JSON-RPC `-32000` with diagnostic data. Transport cancellation
may instead surface through ACP's cancellation response.

| Stable diagnostic code | Meaning and action |
| --- | --- |
| `hive::workspace_tools::arguments` | Omit arguments or pass `{}`; resource selectors are not accepted. |
| `hive::workspace_tools::unsupported` | Discover again and use an advertised tool name. |
| `hive::workspace_tools::scope` | Inspect and repair the owning session's registered workspace and backend. |
| `hive::workspace_tools::owner` | Reconnect to the Hive node owning the session/workspace. |
| `hive::workspace_tools::session` | Use the active attachment; closed/replaced providers cannot execute tools. |
| `hive::workspace_tools::cancelled` | Retry from an active session only if the read-only result is still needed. |
| `hive::workspace_tools::backend` | Inspect owner-side resource-store/backend diagnostics, repair, then retry. |

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
- Gateway `remote_workspace_capability_calls_execute_on_the_owner_during_startup_and_recovery`:
  direct peer-to-owner Git/JJ/optional WorkTrunk capability calls during remote
  startup and restart. This exercises the selected owner's provider, rather than
  routing through an initiating Hive gateway.

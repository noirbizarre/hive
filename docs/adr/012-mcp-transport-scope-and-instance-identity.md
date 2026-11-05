# ADR-012: MCP transport, scope, and instance identity

## Status

Accepted. Defines the executable identity contract for [#93][issue] and shared
stdio runtime for #94, part of the [managed MCP lifecycle epic][epic]. Scope
discovery and agent routing follow in #95 and #96 respectively.

## Context

An MCP transport does not determine who starts the server, how long it lives,
or which sessions can safely share it. Sharing by server name or command alone
can connect a session to another project's configuration or credentials.

### Implementation and specification audit

The existing implementation already handles inline ACP declarations:

| Area | Current evidence and behavior |
| --- | --- |
| Daemon configuration | `crates/hive-daemon/src/config.rs`: `Config`, root/fragment documents and `Profile` configure agents and workspace policies, not MCP servers. Unknown fields are rejected. |
| Native ACPv2 creation | `Broker::create` and `run_agent` in `crates/hive-daemon/src/lib.rs` retain and forward the client's MCP declarations. |
| Recovery | `Record::mcp_servers` in `src/persistence.rs` defaults to empty for legacy records. `Broker::recover` restores saved declarations for the downstream session. The test `recovery_keeps_workspace_roots_and_mcp_connections_alongside_the_original_identity` covers persistence. |
| Live resume | `Broker::resume` attaches to an existing session; incoming MCP declarations are not applied as reconfiguration. |
| Other adapters | `run_v1_agent` and `src/opencode/worker.rs::run_opencode_agent` reject nonempty MCP declarations. Bundled clients do not compose declarations. |
| Capabilities | Hive does not advertise an MCP execution guarantee; per-transport downstream capability validation is incomplete. `unstable_mcp_over_acp` is disabled. |
| Existing identity code | `src/reload.rs::identity` binds OpenCode agent profiles. It is not an MCP reuse key: a password environment-variable name does not identify its current credential value. |

Paths abbreviated as `src/` above are relative to `crates/hive-daemon/`.
See the [compatibility matrix](../compatibility.md#mcp-and-additional-experimental-features)
for the current support status.

The [pinned ACPv2 session-setup specification][acp] describes stdio and HTTP
connection declarations and transport capabilities. Agents connect to those
servers; forwarding is not Hive-owned server management. Unknown/custom
transports preserved by the ACP schema are not an implemented MCP bridge.

The [MCP transport specification][mcp-transports] separates stdio from Streamable
HTTP. Initialization also establishes protocol version, client identity and
capabilities; those inputs can change server behavior and matter for reuse.
HTTP sessions may be stateful. Sharing an HTTP URL is not permission to share
one initialized MCP session.

HTTP forwarding sends the URL and headers to the agent. Hive neither
launches a local HTTP process nor pools HTTP MCP clients. The native ACP
declarations are persisted as supplied, including literal environment/header
credentials. The new contract does not change that existing persistence behavior.

## Decision

### Separate transport, ownership, scope and lifecycle

`hive_daemon::mcp::ServerDefinition` is a checked standalone library schema.
It is **not yet accepted in daemon configuration files** and does not register
or activate servers. Trusted Rust callers can admit resolved identities to the
runtime below; configuration selectors remain future work.

| Dimension | Values and meaning |
| --- | --- |
| Transport | `stdio`: command, ordered args, explicit env. `http`: endpoint and headers. |
| Ownership | `forwarded`: the agent receives connection instructions. `managed`: Hive owns a local stdio process. |
| Scope | `machine`, `project`, `workspace`: maximum eligible sharing boundary. |
| Lifecycle | `delegated`: Hive does not manage lifetime. `on_demand`: first lease starts a managed process, with an idle timeout. |

Forwarded stdio and HTTP require delegated lifecycle. Managed stdio requires
on-demand lifecycle. Managed HTTP is rejected with `hive::mcp::managed_http`;
there is no supported local HTTP launch specification. An externally started
loopback HTTP endpoint remains ordinary forwarded HTTP. HTTP transport never
implicitly starts a process. Agent-profile `http+sse` is a separate vocabulary.

Example of the **standalone contract only**, not daemon TOML syntax:

```json
{
  "scope": "project",
  "ownership": "managed",
  "lifecycle": { "kind": "on_demand", "idle_timeout_seconds": 60 },
  "transport": {
    "kind": "stdio",
    "command": "example-mcp-server",
    "args": ["--read-only"],
    "env": {}
  }
}
```

Unknown fields, malformed process values, ambiguous duplicate HTTP header names,
invalid HTTP(S) URLs/headers, URL userinfo/fragments and inconsistent lifecycle
combinations are rejected. Endpoint validation does not establish reachability
or authorize network access. Plain HTTP is allowed for existing endpoints;
the caller remains responsible for choosing an appropriate transport for secrets.

For on-demand lifetime, zero timeout means stop after the last lease and
in-flight operation; a positive timeout retains an unused instance for that
many seconds. Active leases or operations prevent idle cleanup. A client
disconnect alone does not release a durable session's lease. The runtime below
implements this scheduling contract without claiming exactly-once execution.

### Scope uses the existing resource domain

| Scope | Resolved identity |
| --- | --- |
| `machine` | Execution `NodeId` |
| `project` | Execution `NodeId` and `ProjectId` |
| `workspace` | Execution `NodeId`, `ProjectId` and concrete `WorkspaceId` |

This follows [ADR-011](011-project-and-workspace-domain.md). A project can share
compatible instances across its workspaces. Different worktree Workspaces do not
share workspace-scoped instances. Session subdirectories do not change Workspace
identity, although a changed effective launch cwd can still prevent reuse.

The initial epic/issue vocabulary `machine` / `workspace` / `worktree` maps to
`machine` / `project` / `workspace`. There is no released managed scope syntax
requiring migration. `user` is not a scope alias: execution-node locality and
security identity are independent constraints.

The resolver must supply registered IDs and verify resource ownership on the
execution node. Never derive IDs from cwd, branch names or config filenames,
allocate fresh IDs per lookup, or substitute machine scope when context is
missing. An unmanaged path needs registration before workspace-scoped reuse.

### Compatibility is an opaque structural instance key

`InstanceIdentity::shared` accepts only a complete resolved snapshot. Its private
fields participate in equality:

- Definition namespace/ID and full definition, including ownership and lifecycle.
- Resolved scope and node affinity.
- Resolved executable, ordered argv, complete effective inherited/overridden
  environment and launch cwd.
- Exact effective HTTP endpoint, headers and relevant network configuration
  such as proxy/TLS/client-certificate context.
- Initialization protocol version, client info, capabilities, options and roots.
- Principal, security partition, access-policy generation and trusted external
  credential generations. Actual env/header credential values also participate.

Only display metadata outside the definition/identity may be excluded. Unknown
initialization options remain significant. Map insertion order is irrelevant;
argument order and URL path/query spelling remain significant. Conservative
extra separation is preferable to guessing behavioral equivalence.

Scope grants eligibility, not forced sharing. A machine-scoped server with a
different effective cwd, environment, roots or credentials needs another instance.
Lifecycle policies also participate so one instance cannot acquire conflicting
cleanup policies.

Resolution performs no filesystem, environment, credential-helper or network
operations. `NodePath` carries owning-node path syntax. The resolver must provide
canonical observed paths and the complete effective environment, including
non-Unicode values supported by `OsString`; explicit definition overrides must
match. Syntax checks cannot prove which executable a command resolves to or
whether a caller has supplied truthful resource/credential evidence.

`Credentials::Complete` asserts that effective inputs contain all direct secrets
and external credential sources have trustworthy generations that change on
rotation. Variable names alone are insufficient. Unknown external state is
rejected for shared identity. `InstanceIdentity::private` accepts it with a fresh
nonce so independent admissions cannot reuse it; a clone retains the same private
identity. Changing policy or credentials creates a new key rather than mutating
an admitted instance's snapshot.

Resolved keys and security inputs are nonserializable and have entirely redacted
Debug output. No secret-dependent digest is exposed. Structural equality avoids
an offline credential-guessing oracle and needs no hash-key lifecycle. It is not
an authentication primitive or secure memory erasure. Definition serialization
is deliberately available for configuration; do not treat it as redacted output.

Forwarded HTTP identity describes declaration compatibility only. It does not
authorize pooling the agent's connection or remote MCP session, whose complete
initialization and security state Hive may not know.

### Shared stdio runtime

`Broker::bind_mcp` accepts a fully resolved local managed-stdio identity for a live,
non-closing session. It returns a revocable `SessionMcpLease`; the Broker retains
ownership even if callers drop their handles. Rebinding an equal identity to the
same session returns the existing binding. `Broker::release_mcp` revokes it and
also works while its session is offline. `Broker::mcp_execution_node` supplies the
existing persistent resource node, or one ephemeral node for an in-memory Broker.
The standalone `mcp::runtime::Runtime` API supports other trusted Rust owners.

One process and one initialization handshake serve equal identities. Startup is
single-flight. The runtime launches the exact resolved executable, arguments,
cwd and complete environment, without inheriting new daemon environment values.
The pinned ACP SDK's standalone MCP roles and JSON-RPC engine provide request IDs
and concurrent response correlation. This is not MCP-over-ACP or an agent route.

The initial runtime profile supports protocol `2025-11-25`, object client info
with name/version, and client capabilities `{}` or `{"roots": {}}`. Initialization
options are extra top-level parameters; reserved protocolVersion/clientInfo/
capabilities collisions are rejected. Unknown accepted options remain significant
and are sent unchanged. The server must negotiate the admitted protocol version.
Readiness follows successful initialization and the initialized notification.

Immutable roots are served as file URIs through `roots/list`, not an invented
initialize field. Server ping is supported; sampling, elicitation and unknown
requests receive method-not-found. Notifications are not broadcast to sessions.
The checked request API permits ping, tools list/call, resources list/templates/
read and prompts list/get, subject to server capabilities. It excludes lifecycle,
subscription and task operations whose cross-session ownership is not yet defined.
Task augmentation on ordinary tool calls is also rejected, since task creation
does not establish that the underlying work has finished. Oversized local requests
are rejected before publication without terminating other callers' connections.

Cancellation before publication sends no request. After publication it sends MCP
`notifications/cancelled`, translating the SDK's ACP cancellation notification.
The caller can receive cancellation immediately, but the operation hold and
response consumer survive until a response or process termination. Late responses
are consumed. An ignored cancellation does not kill a shared process or interrupt
other callers, and can therefore prevent idle cleanup indefinitely. There is no
default tool-execution timeout. Applications should not infer that cancellation
prevented a side effect.

Failed startup, process exit, malformed/oversized protocol output and transport
failure produce controlled typed diagnostics. Published operations can have an
uncertain outcome. The runtime never automatically replays them. Existing leases
can start/join a replacement on a later explicit request: restart failures use
250 ms exponential backoff capped at 30 seconds, with at most three starts per
identity in a rolling minute. Cooldowns survive immediate release/reacquisition;
normal idle stops do not count as crash loops. A sustained healthy minute resets
consecutive failures. No background timer starts replacement processes.
If process cleanup cannot be confirmed, the identity stays unavailable and
operation resource protection remains in place; inspect remaining processes
before restarting the daemon, rather than launching a potentially overlapping copy.

Default library policy allows 15 seconds for initialization, 64 outstanding calls
per instance (including startup waiters), and 8 MiB per protocol frame. Busy calls
fail promptly. These bounds do not promise a total-memory cap against unsolicited
notification floods. Policy can be supplied to standalone runtimes in Rust; it is
not daemon TOML configuration. Stderr is drained separately and discarded. Raw
server messages/data and admitted secrets are omitted from public errors, and SDK
payload logging is suppressed within managed connection drivers.

Client disconnect and pilot handoff retain bindings. Worker exit cancels that
session's callers; persistent recoverable sessions retain ownership and reject
offline invocation, while nonpersistent removed sessions release ownership.
Same-daemon recovery can reuse bindings. Resolved identities/bindings are never
serialized or restored after a daemon restart: a trusted resolver must readmit
them. Explicit close revokes bindings and cancels their work without killing
other sessions' instances. Workspace release is deferred while published MCP
operations can still access its files, including operations from bindings that
were explicitly released before close.
Before acknowledging such a close, Hive persists a `closed_pending_cleanup`
record. It is excluded from session recovery after restart while its workspace
holds remain protected until completion is established. A restart cannot recover
the process-local evidence needed to release those holds automatically.

Normal idle cleanup closes stdin, waits up to two seconds for exit, then kills
and reaps the process group/Windows Job if necessary. Daemon shutdown fences
admission and permits five seconds of operation draining before this bounded
termination. Dropped owners also signal cleanup; task abortion kills the group.
Active leases/operations prevent idle cleanup; shutdown and process/transport
failure are separate terminal paths.

Tests in `src/mcp/runtime/tests.rs` cover virtual-time idle races, and
`tests/mcp_runtime.rs` exercises real subprocess sharing, response reordering,
cancellation, lazy restart, diagnostics and Broker session ownership using an
independent native JSON-RPC fixture. Paths are relative to `crates/hive-daemon/`.

## Consequences

- Existing daemon configs, inline ACP declarations and saved records keep their
  behavior. Unknown ACP transports still pass through the existing schema; the
  new managed contract rejects unsupported transports.
- Tests in `crates/hive-daemon/src/mcp/tests.rs` enforce schema validation,
  resource scope, effective configuration/credential isolation and redaction.
- Structural keys retain effective secrets in memory and do not provide a hash
  index. Runtime code may assign opaque handles or add a private keyed index
  later while retaining these compatibility rules.
- Runtime startup/multiplexing (#94) consumes this contract. Authoritative scope
  discovery (#95) and agent routing/HTTP preservation (#96) must also consume it.
  The runtime API does not upgrade advertised ACP capabilities or activate
  managed tools on agent connections.

[issue]: https://github.com/noirbizarre/hive/issues/93
[epic]: https://github.com/noirbizarre/hive/issues/92
[acp]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/session-setup.mdx
[mcp-transports]: https://modelcontextprotocol.io/specification/2025-11-25/basic/transports

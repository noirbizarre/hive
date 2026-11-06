# Hive ACP extensions: remote discovery and control

**Contract version 1 — discovery, selection and routing implemented.** This normative
contract originates in [#77][issue]. Persistent owner Hives implement negotiation,
agent discovery and selection/startup through [#79][discovery], over the remote
Streamable HTTP transport validated in [#78][transport]. [#80][routing] implements
qualified references, read-only resolution and routing through configured owners.
See the [compatibility matrix][matrix] for existing selection/control metadata.

MUST, MUST NOT, SHOULD and MAY express requirements on implementations claiming
this contract.

## Current implementation

Persistent brokers advertise `agents`, `selection`, `control`, `location`, `peers`,
`workspaces` and `profiles` after a compatible
offer. They reuse their stored execution-node ID across restart. In-memory embedded
brokers omit the Hive advertisement. A missing/null offer retains legacy metadata
behavior; an incompatible offer disables extension operations/selectors while
ordinary ACP initialization remains available.

Discovery lists the receiving Hive's configured profiles without probing them.
Capabilities are conservatively `{}`. All clients admitted by the configured
transport share one administrative principal and the same inventory/launch access;
there is no per-user resource policy. Explicit private-network trust or shared-token
authentication is the deployment's access boundary.

Pagination retains one immutable configuration snapshot per inventory kind and connection for five
minutes. A new listing replaces it; each continuation rotates its opaque token.
Completed, expired, superseded and foreign cursors fail rather than restart.
Configuration reload does not mix pages, but discovery never reserves an agent.

Negotiated launches reject malformed Hive metadata, unknown operational fields,
and nonempty additional-directory/MCP declarations before execution. Unmanaged cwd
must be an existing absolute owner-side directory. Configured workspace selection
keeps its existing placeholder-cwd and effective-directory behavior. The negotiated
`workspaces` feature also accepts portable project definitions and provisions them
on the selected owner. Owner-configured processes use existing adapters
and remain owner-supervised after disconnect.

Confirmed pre-create startup failures return `launch_failed`. Once downstream
creation might have been published, response loss or a generic downstream failure
returns `launch_uncertain`, retaining unavailable catalog evidence and workspace
holds. A successfully registered session survives loss of its upstream response.
Neither transport nor peer helper retries session creation.

The reusable Rust peer API is `hive_protocol::remote::Peer`: initialize it on an
existing `Credentials::peer` connection, call `list(ListParams)`, then
`start(agent_id, NewSessionRequest)`. It validates version/features, preserves
standard session responses, and sanitizes remote errors. Ordinary session control
uses the original ACP connection and returned attachment metadata. Reconnect
requires initialization, `resolve(SessionReference)`, then explicit standard resume.
For gateways, `list_peers`, `list_at`, `list_profiles_at`, `start_at` and
`start_profile_at` use explicit owner NodeIds. Prepared creation APIs allow bridges
to install parent-ID mappings before publishing requests. Both built-in clients
use negotiated ACP discovery, owner-scoped launch selection, and qualified
reference resolution while sharing their existing session reducers and controls.

## Baseline and minimal extension surface

This contract targets wire `protocolVersion: 2` with Rust SDK and HTTP adapter
**3.5.0**, schema crate **1.12.0**, and the specification at schema source
revision [`e2d83349`][spec]. These are different version identifiers. The
[baseline audit](compatibility.md#audit-baseline) records provenance and enabled
features. Newer drafts and disabled experimental features are not the baseline.

| Operation | Standard ACP surface | Demonstrated gap / Hive addition |
| --- | --- | --- |
| Discover hosted agent profiles | `initialize` identifies one peer; `session/list` enumerates sessions | Neither enumerates configured profiles without sessions: `_hive/agents/list` |
| Select/start an agent | `session/new` creates a session with cwd and optional launch inputs | No profile selector: existing `_meta.hive.agent`; no separate start RPC |
| Enumerate sessions | `session/list`, including cwd filtering and opaque pagination | Retain this method; add qualified owner references in metadata |
| Locate a retained session across Hives | `session/resume` requires an endpoint-local ID and matching cwd | `_hive/sessions/resolve` maps an owner-qualified reference to the current gateway's route |
| Attach, prompt, cancel, close, configure | Standard `session/*` methods | Existing Hive fencing, prompt keys and replay metadata; no duplicate methods |
| Stream events and ask for decisions | `session/update`, `session/request_permission`, `elicitation/*` | Existing control/interaction contracts; no new event bus |

The pinned [method registry][methods] and [schema][schema] establish these gaps.
Experimental ACP `providers/list` concerns LLM provider configuration, not
executable agent profiles, and Hive does not enable it. A standard method name
does not remove Hive's existing controller requirements.

All custom methods start with `_`; extension data on standard objects stays
inside their `_meta`, following pinned [extensibility rules][extensibility].
There is no separate Hive-to-Hive application protocol.

## Initialization and version negotiation

Both sides use **`capabilities._meta.hive`**, not custom root capability fields.
This is Hive's project namespace, shared with existing `_meta.hive` contracts.
It applies to the initialized connection, not every downstream agent.

| Side / field inside `capabilities._meta.hive` | Type / requirement |
| --- | --- |
| Client `versions` | Required nonempty array of distinct positive integers; supported contract versions |
| Server `version` | Required positive integer selected from the client's offer |
| Server `node` | Required persistent execution-node ID identifying this Hive authority |
| Server `features` | Required object of the feature markers below |

The server MUST select the highest mutually supported version. If the key is
absent/null or no version is common, it MUST omit its Hive advertisement and
MAY still initialize standard ACP successfully. A client MUST reject a server's
unoffered version as an invalid extension negotiation and disable Hive operations.
A malformed recognized advertisement returns `-32602`; initialization remains
unsuccessful. An unknown offered version is not malformed.

| `features` key | Marker meaning / dependencies |
| --- | --- |
| `agents` | `_hive/agents/list` is implemented |
| `selection` | `_meta.hive.agent` is consumed by `session/new`; requires `agents` and `control` |
| `workspaces` | Workspace intent and portable project transfer are consumed by `session/new`; requires `selection` |
| `profiles` | `_hive/profiles/list` and `_meta.hive.profile` selection are implemented; requires `selection` |
| `location` | Qualified session metadata and `_hive/sessions/resolve` are implemented; requires `control` |
| `peers` | Configured peer enumeration, peer-scoped inventories and explicit peer launch selection; requires `location` and `selection` |
| `control` | Existing Hive controller, prompt-key and replay contracts linked below are implemented |

Each recognized marker is `{}` when supported, omitted/null otherwise; other
values are invalid. Advertisements MUST describe behavior actually available to this
connection. `control`, `selection` and `location` require standard
`capabilities.session` support. Advertising `session: {}` still commits the
server to ACP's seven baseline session methods, not merely extension support.
Other standard optional capabilities must be negotiated separately.

Example offer and selection (the `info` values are illustrative):

```json
{
  "jsonrpc": "2.0", "id": 1, "method": "initialize",
  "params": {
    "protocolVersion": 2,
    "info": {"name": "hive", "version": "example"},
    "capabilities": {"_meta": {"hive": {"versions": [1]}}}
  }
}
```

```json
{
  "jsonrpc": "2.0", "id": 1,
  "result": {
    "protocolVersion": 2,
    "info": {"name": "hive", "version": "example"},
    "capabilities": {
      "session": {},
      "_meta": {"hive": {
        "version": 1,
        "node": "node-11111111-1111-4111-8111-111111111111",
        "features": {"agents": {}, "selection": {}, "location": {}, "control": {}}
      }}
    }
  }
}
```

Clients MUST check the selected version and each required feature before use.
A plain ACP peer might instead return `"capabilities": {"session": {}}`.
In that case Hive discovery/selection/location are disabled: the client can
offer standard ACP behavior, but MUST NOT strip a requested selector and launch
the peer's default agent instead. Existing local clients using legacy Hive
metadata are outside this new negotiation guarantee.

## Named execution profiles: `_hive/profiles/list`

With the `profiles` feature, list named execution setups using the same `cursor`
and `limit` contract as agent discovery. Results contain `profiles`, nullable
`defaultProfile`, and optional `nextCursor`. Each profile has `id`, `name`, member
ID arrays `agents` and `projects`, nullable `defaultAgent` and `defaultProject`,
`isDefault`, and a `workspaces` map keyed by project ID. Workspace summaries expose
only `{ "template": "name" }` or `{ "policy": "kind" }`, never private directories
or policy settings. Profile and agent cursors are independent and not interchangeable.

Send `_meta.hive.profile` as a nonempty string in `session/new`, optionally with
`agent` and `workspace`. Profile-only requests resolve owner-side defaults against
one admission snapshot. Unknown profiles, nonmember selections and ambiguous
defaults return `hive::remote::invalid_profile`. The [configuration rules](configuration.md#selection-and-overrides)
define precedence and workspace behavior. Selectors are consumed by Hive.

The Rust `Peer` offers `list_profiles(ListParams)` and
`start_profile(profile_id, optional_agent_id, request)`. Both require the advertised
feature; unsupported owners never receive a silently downgraded launch. Existing
`list` and `start` continue to operate on concrete agent IDs. A configured
`default_profile` also applies when no profile selector is supplied.

## Configured peers and explicit selection

With `peers` negotiated, `_hive/peers/list` accepts `{}` and returns
`{"peers":[{"owner":NodeId,"alias":"workstation","name":"Workstation"}]}`.
`name` can be null. This enumerates statically configured registrations without
probing. Responses omit endpoints, credential selectors and token values.

The existing `_hive/agents/list` and `_hive/profiles/list` accept optional
`peer: NodeId` alongside `cursor` and `limit`. Omission selects the receiving
Hive's local inventory; a present value selects a verified configured owner.
Agent/profile IDs remain opaque and owner-scoped. Cursors are scoped to the
upstream connection, owner, inventory kind and live owner link. Replacing an
owner link invalidates continuation rather than silently restarting discovery.

Remote creation uses standard `session/new`, for example:

```json
{
  "cwd": "/srv/project",
  "_meta": {"hive": {
    "peer": "node-11111111-1111-4111-8111-111111111111",
    "agent": "coder"
  }}
}
```

`peer` MUST identify a configured owner NodeId, never a local alias or URL.
An explicit `agent` or `profile` is required for remote creation; profile-plus-agent
selection retains normal owner-side override rules. The gateway consumes `peer`
and forwards existing selectors via `Peer`, without applying local defaults or
filesystem/workspace admission. Unavailable peers never fall back to local agents.
Every new owner connection verifies its advertised NodeId before use. Registration
and reload behavior is documented in [configuration](configuration.md#remote-hive-peers).

## Agent discovery: `_hive/agents/list`

Client-to-Hive, connection-scoped request. It lists authorized **configured**
profiles at the receiving Hive, not arbitrary profiles recursively discovered
through its peers. An initiating Hive can query each configured peer separately.
It MUST NOT start or probe processes, provision a workspace or attach a session.

| Request field | Type / behavior |
| --- | --- |
| `cursor` | Optional opaque nonempty string; omitted starts a listing |
| `limit` | Optional integer 1–100; default 100; maximum entries per response |

| Result field | Type / behavior |
| --- | --- |
| `agents` | Required array of profile objects below, at most `limit` entries |
| `defaultAgent` | Required nonempty profile ID or null if no authorized default exists |
| `nextCursor` | Optional nonempty string; omitted when complete |

Cursor continuation MUST stay within one authorization-filtered inventory
snapshot. Expired or invalid cursors fail with `-32602`, never restart silently.
The default can occur on another page. IDs are unique within the snapshot;
sorting by ID makes a fresh listing deterministic. Reload can invalidate a
cursor; inventory is not a reservation against reload or launch failure.

| Profile field | Type / behavior |
| --- | --- |
| `id`, `name` | Required nonempty strings: opaque configured ID and display name |
| `kind` | Required string: configured adapter kind, e.g. `acp` or `opencode-server` |
| `transport` | Required string: declared downstream transport, not a client connection instruction |
| `versions` | Required array of distinct downstream version tags: `"v1"` or `"v2"`; empty for non-ACP adapters |
| `isDefault` | Required boolean; consistent with `defaultAgent` |
| `capabilities` | Required object; optional `session` contains standard v2 session capability declarations known to be supported through Hive for this profile |
| `launch` | Required object: `cwd` is `"required"`; `workspace` is `"unsupported"` or `"configured"` |

An omitted capability means unknown/not promised, not proven absence in the
agent. No downstream capability may be copied unless Hive implements its
end-to-end behavior. Unknown informational fields/kinds may be displayed
generically. They MUST NOT become instructions to execute a command or connect
to an endpoint. `launch.workspace: "configured"` signals that owner-configured
workspace policy can apply; it does not negotiate remote workspace provisioning.

The safe projection extends the existing HTTP public profile inventory.
Commands, arguments, environment, headers, credentials, private endpoint URLs
and filesystem paths MUST NOT be included.

```json
{"jsonrpc":"2.0","id":2,"method":"_hive/agents/list","params":{"limit":100}}
```

```json
{
  "jsonrpc": "2.0", "id": 2,
  "result": {
    "defaultAgent": "coder",
    "agents": [{
      "id": "coder", "name": "Coding agent", "kind": "acp",
      "transport": "stdio", "versions": ["v2"], "isDefault": true,
      "capabilities": {},
      "launch": {"cwd": "required", "workspace": "unsupported"}
    }]
  }
}
```

## Selection and startup: standard `session/new`

With `selection` negotiated, `_meta.hive.agent` is an optional nonempty profile
ID from the receiving Hive's inventory. An explicit unknown or forbidden ID
MUST fail, never select another profile. Omitting the Hive selector entirely
uses the authorized configured default, or fails if none exists. For compatibility
with the existing parser, a Hive metadata object MUST contain `agent` unless it
contains a supported workspace selector permitting default selection.

The standard `cwd` remains required and absolute in the **owner's** filesystem.
Additional directories and MCP declarations require their standard advertised
support; discovery does not imply it. A profile is pinned at launch admission;
configuration reload MUST NOT repoint an existing session to a different agent.

```json
{
  "jsonrpc": "2.0", "id": 3, "method": "session/new",
  "params": {"cwd": "/srv/project", "_meta": {"hive": {"agent": "coder"}}}
}
```

The owner validates launch inputs and starts/supervises the configured adapter
using its local transport. Success returns the standard `sessionId` and existing
Hive attachment metadata. If `location` is enabled, it also returns the qualified
reference specified below. No separate process lifecycle method is introduced.

Existing [workspace selectors](hive-api.md#session-launch-selectors) and native
adoption retain their contracts. Neither is implied by `selection`.

### Provisioning on the selected owner

With `workspaces` advertised, `session/new` can carry a `new` selector with an
explicit project ID and optional portable `definition`:

```json
{"cwd":"/","_meta":{"hive":{"agent":"coder","workspace":{
  "kind":"new","project":"demo","policy":"task",
  "definition":{"name":"Demo","repository_source":"https://example.test/demo.git"}
}}}}
```

The receiving owner resolves the named policy, provisions through its local
workspace service, and supplies the observed cwd to its agent. Neither client nor
initiating Hub needs a checkout or may inspect the returned cwd locally. Without
this capability the peer helper rejects workspace intent instead of dropping it.

An absent project definition is registered under the exact supplied ID. Equal
configured or managed definitions are reused; different definitions fail with
`project_conflict`, never overwrite or allocate a suffixed identity. Without a
definition, the owner resolves its existing project. Registration survives later
provisioning or agent startup failure. Policies, filesystem overrides, Git
credentials and WorkTrunk configuration remain on the owner. Project preparation
argv is stored metadata, not a new command-execution facility; owner-approved
WorkTrunk hooks retain their normal lifecycle.

Workspace context includes `owner`, `project`, `workspace`, and `effectiveCwd`.
An attach selector may include `owner`; a foreign owner is rejected before local
resource inspection. Successful managed launches and resumes also include
`_meta.hive.session: {"owner": NodeId, "id": SessionId}`. Negotiated unmanaged
sessions now carry the same reference. Old contexts without `owner` remain readable.

Acquisition/backend failures return `provisioning_failed`, interrupted effects
return `provisioning_uncertain`, and invalid owner project/policy inputs return
`invalid_workspace`. These are distinct from downstream agent startup failures.
WorkTrunk lifecycle commands and activity markers run only on the execution owner.

`session/new` is not made idempotent by prompt keys. If a response is lost after
process creation, the caller MUST report uncertain startup and inspect the owner
catalog before an explicit retry; it MUST NOT blindly create another session.

## Session identity, catalog and location

A qualified reference is `{"owner": NodeId, "id": SessionId}`. Both are required
nonempty strings. `owner` is the existing persistent execution `NodeId`
(`node-<UUID>`); `id` is the owner's opaque Hive session ID, not its native agent
session ID. Clients MUST treat both as opaque and MUST NOT infer routes from them.

Each owning Hive MUST retain its node identity across restart and endpoint
changes; separately authoritative registries MUST NOT share one node identity.
A gateway alias is scoped to that gateway, whereas the qualified reference is
stable across gateways. URLs, display names and profile IDs are not owner IDs.

With `location` negotiated, each standard `session/list` entry and successful
`session/new`/`session/resume` response MUST include
`_meta.hive.session: {"owner": ..., "id": ...}` alongside existing Hive metadata.
The standard `sessionId` is usable at the **responding gateway**. List entries
retain standard cwd/filter/cursor semantics; no separate session-list RPC exists.
This metadata does not upgrade Hive's currently partial pagination support.

Negotiated `session/list` merges local sessions with directly configured owners'
catalogs. Gateways request `_meta.hive.ownerOnly: true` on the standard list method
to enumerate only that peer's owned sessions; it requires negotiation. This prevents
cycles from recursively aggregating catalogs. No transitive owner discovery occurs.
On an owner outage or registration removal, known cached entries carry
`_meta.hive.cached: true` and `availability: "owner_unavailable"`; response metadata
includes `partial: true` and `unavailableOwners: [NodeId, ...]`. Missing connectivity
never proves absence. Successful authoritative listings supersede cached presence.
Absence is reconciled only for complete listings within their cwd scope, without
revoking newer positive observations. Confirmed close or authoritative not-found
also retires cached presence. The alias remains retained for stable-reference
diagnostics, including after gateway restart, but does not reappear as an outage entry.

Gateway aliases and qualified references/cwd are durable routing metadata, stored
separately from local recovery records. Controller tokens, replay journals and
process ownership stay on the owner. Gateway restart requires fresh verification
and explicit attachment; it never starts a replacement remote worker.

For example, creation at the owner returns the following response (other existing
attachment snapshots are omitted here for clarity):

```json
{
  "jsonrpc": "2.0", "id": 3,
  "result": {
    "sessionId": "hive-example",
    "_meta": {"hive": {
      "session": {
        "owner": "node-11111111-1111-4111-8111-111111111111", "id": "hive-example"
      },
      "generation": 1, "token": "opaque-example", "nextSeq": 1,
      "running": false, "pilot": true, "role": "pilot"
    }}
  }
}
```

Standard `session/list` with `params: {}` exposes the same reference without a
controller token. Through a routing gateway, its result can instead be:

```json
{
  "jsonrpc": "2.0", "id": 6,
  "result": {"sessions": [{
    "sessionId": "hive-route-example", "cwd": "/srv/project",
    "_meta": {"hive": {"session": {
      "owner": "node-11111111-1111-4111-8111-111111111111", "id": "hive-example"
    }}}
  }]}
}
```

### `_hive/sessions/resolve`

Client-to-Hive, connection-scoped, authorized read-only lookup of one retained
reference through the receiving gateway's configured routes.

| Field | Type / behavior |
| --- | --- |
| Request `session` | Required qualified reference; no top-level `sessionId` |
| Result `session` | Required identical qualified reference |
| Result `sessionId` | Required nonempty gateway-local ID usable in standard ACP session requests |
| Result `cwd` | Required absolute owner-side cwd to use for resume |

The nested request is deliberate: the pinned HTTP adapter treats a top-level
`params.sessionId` as session transport scope, which would make a location lookup
depend on an attachment that does not yet exist.

```json
{
  "jsonrpc": "2.0", "id": 4, "method": "_hive/sessions/resolve",
  "params": {"session": {
    "owner": "node-11111111-1111-4111-8111-111111111111", "id": "hive-example"
  }}
}
```

```json
{
  "jsonrpc": "2.0", "id": 4,
  "result": {
    "session": {
      "owner": "node-11111111-1111-4111-8111-111111111111", "id": "hive-example"
    },
    "sessionId": "hive-route-example", "cwd": "/srv/project"
  }
}
```

The gateway may record a routing alias but MUST NOT start/restore an agent,
attach a client, take control, provision a workspace or create a replacement.
It MUST verify the configured peer's identity before reporting a usable route.
An unavailable owner is not proof a session is missing; a stale mapping must
fail rather than target a different session. Unknown owners are not dynamically
discovered. Resolution returns no credentials or redirect URL, and clients MUST
NOT construct an endpoint from a returned identity.

The result is a location, not a liveness/resumability guarantee. A subsequent
standard resume can fail if the owner disconnects, authorization changes or
retained history cannot be recovered. After reconnect, resolve the retained
reference again rather than trusting an expired gateway alias.

## Standard control and authority across hops

The owning Hive remains authoritative for supervision, controller generation,
prompt-key deduplication, cancellation and recovery. A gateway routes control; it
does not acquire independent authority to restart or replace a remote session.

Use the existing [control/replay](compatibility.md#control-and-replay),
[mutation ownership/idempotency](compatibility.md#mutation-ownership-and-idempotency)
and [interaction](compatibility.md#interaction-capabilities-and-fallback) contracts.
Pilot tokens are private to an attachment; observers never receive them.
Generation/token checks and prompt keys still apply to standard mutations.
No new control method, event type or distributed lock is defined here.

Gateways MUST map connection-local JSON-RPC/interaction IDs and attachment tokens,
not copy them blindly across hops. Owner-issued authority and invalidation must
fence every mapped controller; an initiating Hive cannot override owner takeover.
Updates and replay cursors must remain associated with the original session and
ordering domain. Unsupported replay positions fail, never imply complete history.

For the example flow, retain the qualified reference from new/list, disconnect,
resolve it as above, then attach with standard ACP:

```json
{
  "jsonrpc": "2.0", "id": 5, "method": "session/resume",
  "params": {
    "sessionId": "hive-route-example", "cwd": "/srv/project",
    "replayFrom": {"type": "start"}, "_meta": {"hive": {"mode": "observe"}}
  }
}
```

Explicit takeover uses the existing pilot resume contract. Subsequent prompts
carry the returned generation/token and a prompt key; cancellation, close,
configuration and permission/elicitation use their existing standard methods.
Transport loss MUST NOT implicitly close the owner's session, redispatch a
prompt, transfer control or silently create a replacement.

## Errors, unknown extensions and evolution

These errors apply to the negotiated contract; legacy handlers retain their
existing payloads. Standard ACP errors retain their standard
meaning. An unrecognized method returns `-32601`; unrecognized notifications
are ignored without a response. Version 1 defines no new notifications.

Recognized contract failures use the following numeric codes and diagnostic
identifiers in `error.data.code`. `error.data.help` is a required actionable
string. `error.message` describes the specific failure without private inputs.

| RPC code | Diagnostic identifier | Meaning / caller action |
| --- | --- | --- |
| `-32602` | `hive::remote::invalid_params` | Malformed known parameters/advertisement or expired cursor; correct inputs/restart listing |
| `-32060` | `hive::remote::unsupported` | Known feature not negotiated; inspect initialization, do not retry with dropped selectors |
| `-32602` | `hive::remote::unknown_agent` | Unknown authorized profile/default absent; refresh inventory and select explicitly |
| `-32060` | `hive::remote::forbidden` | Operation not authorized; request owner-side access |
| `-32602` | `hive::remote::invalid_workspace` | Owner rejected cwd, roots or workspace inputs; select permitted owner-side workspace |
| `-32060` | `hive::remote::launch_failed` | Confirmed launch failure; inspect sanitized owner diagnostics before retry |
| `-32060` | `hive::remote::launch_uncertain` | Launch outcome unresolved; inspect catalog before explicit retry |
| `-32060` | `hive::remote::project_conflict` | Supplied definition differs; explicitly update the owner or select another ID |
| `-32060` | `hive::remote::provisioning_failed` | Inspect owner project, credentials, backend and workspace operations |
| `-32060` | `hive::remote::provisioning_uncertain` | Reconcile owner workspace effects before an explicit retry |
| `-32060` | `hive::remote::session_not_found` | Authorized authoritative lookup found no session; refresh owner catalog |
| `-32060` | `hive::remote::owner_unavailable` | No verified configured owner connection; restore connectivity/configuration |
| `-32060` | `hive::remote::stale_route` | Mapping no longer valid; resolve qualified reference again |
| `-32060` | `hive::remote::agent_unavailable` | Session is retained but its agent is unavailable; inspect owner recovery and explicitly resume |

A transport loss cannot deliver a JSON-RPC error; the caller reports equivalent
local uncertainty. Servers MAY hide forbidden profile/session existence as not
found, consistently with their discovery visibility policy.

```json
{
  "jsonrpc": "2.0", "id": 4,
  "error": {
    "code": -32060, "message": "The session owner is unavailable",
    "data": {
      "code": "hive::remote::owner_unavailable",
      "help": "Restore the configured peer connection, then resolve the retained reference again."
    }
  }
}
```

Unknown informational response fields and capability keys MUST be ignored.
Unknown operational fields inside Hive requests/selectors MUST be rejected with
`invalid_params`, not silently stripped; standard ACP fields retain ACP's own
forward-compatibility rules. Invalid recognized values never fall back to an
unknown-value interpretation. Breaking operational changes require a new integer
contract version; additive informational fields do not. New optional operations
require their own negotiated feature. Diagnostic identifiers are stable public
identifiers and MUST NOT be renamed within a contract version.

## Security and deployment boundary

Discovery and resolution require authorization because profile names, session
existence and cwd can disclose private information. The owner filters inventory
and lookup responses to the caller's allowed resources. Authorization to list
does not imply permission to start, observe, take over or mutate a session.

Selection can start configured processes and access workspaces. The owner MUST
validate cwd, extra roots, MCP declarations, workspace policy and provisioning
hooks before side effects. Client-provided paths are owner-side inputs, never
instructions to access the initiating machine. A profile ID is not permission
to inject arbitrary executable configuration. Errors/logs must omit credentials,
tokens, raw environment and subprocess output; cwd is disclosed only in authorized
session responses where needed for standard resume.

Peers and endpoint credentials are explicitly configured. A private network
such as Tailscale or NetBird provides connectivity, not ACP capability or
operation authorization. The deployment/transport contract chooses authenticated
peer identity or explicitly trusts clients admitted by its network topology;
owner-side operation authorization remains distinct from transport access.
This document introduces no tunnel, VPN or authentication RPC. Pilot fencing is
not authentication and browser-origin checks are not login.

Production Hive defaults to loopback and permits explicitly opted-in remote
binding with optional shared-token authentication through [#78][transport].
Discovery, selection and session routing additionally require extension negotiation. See
[transports](transports.md), [deployment](operations.md#private-network-access)
and [ADR-006](adr/006-local-operational-http-and-acp-boundary.md).

## Implementation conformance checklist

The combined #79/#80 conformance flow is:
initialize, discover, select/new, list the qualified reference, disconnect,
resolve through a gateway, resume and perform ordinary control. They must also
cover unsupported/absent versions and features, unknown requests/notifications,
invalid selectors/workspaces, inventory pagination and reload, filtered discovery,
launch failures and lost responses, unavailable owners, stale routes, takeover
fencing, prompt deduplication, and reconnect without duplicate execution.

Tests must prove discovery/resolve have no execution or control side effects and
that gateway aliases cannot change owner identity. #78 must validate the nested
resolver payload's connection scope over the pinned HTTP adapter. Remote workspace
provisioning evidence for [#88][workspace] lives in the workspace gateway tests;
Built-in client coverage includes owner selection, unavailable catalogs, remote
launch paths, streamed plans/interactions, cancellation, and qualified reconnect.

Implemented #79 evidence lives in `crates/hive-daemon/tests/support/remote_discovery.rs`,
the `remote::tests` modules, and
`negotiated_remote_discovery_starts_bee_and_preserves_standard_control_after_disconnect`
in `crates/hive-daemon/tests/bee.rs`. It covers negotiation, safe inventory/pagination,
reload, strict launch admission, v1/v2 selection, launch failure/uncertainty,
owner-side cwd, standard control and explicit reattachment without duplicate creation.
`crates/hive-daemon/tests/support/remote_routing.rs` exercises two real brokers,
owner HTTP transport, both upstream transports and both ACP adapters: retained
identity and receipts across gateway restart, owner takeover fencing, translated
elicitation parents, lost acknowledgements, unreachable/replaced owners and
retained unavailable agents. Configuration and alias-collision tests live in
the daemon's `config::tests` and `routing::tests` modules.

[issue]: https://github.com/noirbizarre/hive/issues/77
[discovery]: https://github.com/noirbizarre/hive/issues/79
[routing]: https://github.com/noirbizarre/hive/issues/80
[transport]: https://github.com/noirbizarre/hive/issues/78
[workspace]: https://github.com/noirbizarre/hive/issues/88
[matrix]: compatibility.md#hive-extensions-inventory
[spec]: https://github.com/agentclientprotocol/agent-client-protocol/tree/e2d83349bc9e28a4059ea3acefa1c91c966f74e2
[methods]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v2/meta.json
[schema]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/schema/v2/schema.json
[extensibility]: https://github.com/agentclientprotocol/agent-client-protocol/blob/e2d83349bc9e28a4059ea3acefa1c91c966f74e2/docs/protocol/v2/extensibility.mdx

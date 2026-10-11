# Architecture

The daemon (`crates/hive-daemon`) owns the only shared in-memory session broker. The
official `AcpHttpServer` mounts ACP at `/acp`: GET with WebSocket upgrade is a
persistent bidirectional JSON-RPC channel; POST plus per-connection and
per-session GET SSE streams form the HTTP alternative. Both create the same
v2 agent handlers against the same broker. `/hive/v1/*` contains operational
HTTP endpoints and a same-origin browser asset route, not session RPC methods.

The `hive` executable lives in `crates/hive-cli` and depends on the daemon and
TUI crates; neither library depends on the CLI or on the other.
`hive-core` defines transport-free Hive session IDs and lifecycle vocabulary.
`hive-protocol` pins SDK integration and shared native status access. The daemon
owns a registry of inert agent profiles loaded from one XDG TOML file, and the
broker maps each Hive session ID to its selected profile, one downstream
process/connection, and downstream session ID. A downstream worker and session
outlive any attached upstream client. An atomic per-session control state stores
the current pilot generation/token, attached observer recipients, active turn,
prompt-key outcomes, and a bounded log of all emitted updates. `session/resume`
validates replay, atomically rotates pilot control and adds an event recipient;
it never contacts the downstream agent to recreate the conversation. Only an
explicit session close, downstream failure, or daemon shutdown terminates it.

The downstream version and transport are selected **per session** from its
profile declaration (v2 preferred over v1). The pinned SDK supports stdio,
WebSocket and HTTP/SSE. Each session establishes its own connection (and its
own process for stdio); there is no speculative probing or fallback after
initialization errors. v1
text chunks are adapted to v2 agent chunks and the v1 prompt response produces
a v2 idle state. The transport exposed to clients remains v2 regardless.

The full-screen TUI uses SDK `Client.v2()` / `HttpClient` for ACP session
selection, creation and interaction. It can observe without taking control,
relinquish pilot while observing, or take over by Hive session ID, and leaves
the session alive on exit. The browser implements the ACPv2
WebSocket JSON-RPC boundary. It lists daemon-owned sessions, attaches as an
observer using the listed cwd, and takes control when the user starts typing
a prompt. It renders sequenced replay and live events through the same
conversation reducer; reconnect observes the original session rather than
creating another. Pending agent input follows the pilot to whichever client
currently owns control.
ZJHive uses Zellij's host WebAccess permission for one-shot daemon status;
Zellij's `web_request` is not a persistent WebSocket or SSE transport.

The current security boundary is one trusted local OS user. Session routing
checks the exact Hive session ID and downstream ID. Mutating requests also
require the current control generation and opaque token. A new controller
fences the old one, including after a network partition; disconnect leaves the
downstream running until explicit takeover. The event log retains the latest
256 updates (including token-free control changes) with contiguous sequence
numbers. A replay cursor outside the
available range fails without changing control. In-memory retries and retained
events survive client disconnection, **not daemon restart**; no durable storage
or remote security policy is provided. Each attached observer receives live
updates but never an ownership token. Pending permission/elicitation requests
follow the pilot across takeovers and expire if nobody answers.
The daemon exclusively locks a private per-user runtime file before binding;
another `hive daemon` invocation cannot own a different port for the same user.
Both ACP session access and Hive administration are loopback-only, with no
per-client authentication; an untrusted local process can access both.

# Architecture

The daemon (`crates/hive-daemon`) owns the shared session broker and durable catalog. The
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
process/connection, and persisted downstream session ID. A downstream worker and session
outlive any attached upstream client. An atomic per-session control state stores
the current pilot generation/token, attached observer recipients, active turn,
prompt-key outcomes, and a bounded log of all emitted updates. Upstream `session/resume`
validates replay, atomically rotates pilot control and adds an event recipient;
does not contact the downstream agent for a live session. At daemon startup,
Hive instead reattaches through the original downstream ID using the agent's
ACP recovery method; no new downstream session or prompt is sent. Unexpected
worker exits leave an offline catalog record. Explicit close deletes that
record, while daemon shutdown preserves it for reattachment.

The downstream version and transport are selected **per session** from its
profile declaration (v2 preferred over v1). The pinned SDK supports stdio,
WebSocket and HTTP/SSE. Each session establishes its own connection (and its
own process for stdio); there is no speculative probing or fallback after
initialization errors. v1
text chunks are adapted to v2 agent chunks and the v1 prompt response produces
a v2 idle state. The transport exposed to clients remains v2 regardless.

The full-screen TUI uses the daemon session catalog for discovery and SDK
`Client.v2()` / `HttpClient` for ACP session creation and interaction. It
refreshes summaries periodically and preserves session identity across picker
updates. It can observe without taking control,
relinquish pilot while observing, or take over by Hive session ID, and leaves
the session alive on exit. The browser implements the ACPv2
WebSocket JSON-RPC boundary. It lists daemon-owned sessions, attaches as an
observer using the listed cwd, and takes control when the user starts typing
a prompt. It renders sequenced replay and live events through the same
conversation reducer; reconnect observes the original session rather than
creating another. Pending agent input follows the pilot to whichever client
currently owns control.
The browser rail observes catalog revisions via SSE and reconciles full
snapshots with locally visited sessions; its hide/show state is presentation
only. Neither client stores a second authoritative session catalog.
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
events survive client disconnection; restart retains session IDs and prompt
reservations but loses replay history and activity snapshots. A SQLite WAL
database at `$XDG_STATE_HOME/hive/sessions.sqlite3` (or
`$HOME/.local/state/hive/sessions.sqlite3`) keeps the catalog in per-record
transactions. Migrations use SQLite `user_version`; a newer schema refuses to
start rather than overwriting data. Back up the database together with its WAL
file only while Hive is stopped (or through SQLite's backup API). Crash-safe
cursor block reservations expose gaps rather than reusing sequence numbers;
an interrupted prompt requires explicit user acknowledgement before new work.
Explicit closes remove the record immediately; orphaned/lost records without
uncertain work are pruned after 90 days without activity. Unavailable sessions
and records with interrupted work remain until explicitly resolved.
Storage errors fail the affected operation or fence its session. Each attached observer receives live
updates but never an ownership token. Pending permission/elicitation requests
follow the pilot across takeovers and expire if nobody answers.
The daemon exclusively locks a private per-user runtime file before binding;
another `hive daemon` invocation cannot own a different port for the same user.
Both ACP session access and Hive administration are loopback-only, with no
per-client authentication; an untrusted local process can access both.

# Sessions and handoff

A Hive session has a stable `hive-…` ID and a selected agent profile. Its lifetime
is independent of any terminal tab, browser page, or ACP client connection.

## Create a session

Start the daemon, then launch the [TUI](tui.md) from your project directory.
Press `Ctrl-N`, choose a profile, and press Enter. The daemon starts the
downstream worker and records the agent's own session ID.

The session catalog lists sessions across clients and directories. Display
titles come from the first accepted nonempty prompt. `unknown` activity means
the agent has not provided a trustworthy running/idle state, not that it is idle.

## Pilot and observers

Each session has at most one **pilot**: the client allowed to prompt, cancel,
close, and answer session-scoped agent questions. Other attached clients are
**observers** and can read updates without changing the session.

Taking control rotates the session's control credentials. A previous pilot
immediately loses mutation rights, even if its connection remains open. Taking
control does not restart or cancel work already running.

These roles arbitrate Hive clients. They do not identify people or authenticate
against other processes running as the same local user.

## Move between clients

1. Create or open a session in the TUI.
2. Run `hive web` and select that same session in the browser rail.
3. Read its updates as an observer.
4. Type in the browser composer to request control. Wait for confirmation before sending.
5. Press `Ctrl-T` in the TUI to take control back when needed.

In the TUI overview, Enter attaches as pilot and `o` observes. `Ctrl-H`
relinquishes control while continuing to observe.

## Prompt, cancel, detach, and close

| Action | Effect |
| --- | --- |
| Send a prompt | Starts work when the pilot owns an idle, usable session |
| Cancel | Asks the agent to stop foreground work |
| Detach / quit client | Leaves the downstream session and work running |
| Close session | Explicit protocol operation removing Hive's record, refused while work is active |
| Stop daemon | Ends its workers/connections and preserves recovery records |

A prompt response acknowledges acceptance. Wait for the agent's idle state to
know foreground work ended. A second distinct prompt is refused while work is
active. Hive's CLI does not provide a session-close subcommand.

## Reconnect and missing history

Hive retains the latest **256 updates** for incremental reconnects. Text chunks, control
changes, activity, and usage all consume this window. It is not a transcript
archive and does not represent 256 messages or turns. Fresh selection/page reload
uses a separate in-memory transcript snapshot, so wrapping this window does not
discard conversation prefixes from the view.

If retained history is available, reconnect replays it and then continues live.
If the cursor expired, the client reports a gap instead of pretending history is
complete. Choose live-only attachment explicitly (`Ctrl-R` in the TUI, or the
browser's attach-without-history action).

Live-only attachment restores current activity, not missing conversation text.
Reload the browser page or restart the TUI to rebuild the view from a fresh
transcript snapshot instead. Reselecting a TUI session retains its existing cursor.
A daemon restart loses the incremental window and rebuilds the transcript via
ACPv1 `session/load` or ACPv2 `session/resume` with replay from the start.
An agent that cannot replay history or no longer retains the session returns
an actionable diagnostic. Hive keeps the original session identity. See
[restart and recovery](../operations.md#restart-and-recovery).

## Uncertain prompts and interrupted work

A lost prompt acknowledgement does not prove the agent did nothing. Hive clients
use stable prompt keys so an identical retry does not dispatch the same work
twice. Do not submit a new prompt just to compensate for a lost acknowledgement.

The Web UI offers an explicit uncertain-prompt retry action. After a
daemon restart, an interrupted key remains reserved and is never resent.
Inspect the agent's state first. If the recovered session requires acknowledgement,
use `Ctrl-Y` in the TUI or the browser's recovery button to allow **new** work.
This does not certify that old work completed or undo any effects it had.

## Permissions and input

Session-scoped questions follow the pilot across handoff. Request-scoped input
belongs to the client with the original live request and does not move simply
because another client takes control.

Answer explicitly. Missing UI support, invalid input, cancellation, and timeout
never imply approval. Human decisions have a bounded deadline. See each client
guide for form and URL controls, or the
[interaction contract](../acp-compatibility.md#interaction-capabilities-and-fallback).

# Browser client

Start the daemon, then run:

```sh
hive web
```

The browser opens <http://127.0.0.1:8765/ui>. Assets are embedded in
Hive and served by the daemon. No separate frontend process is needed.
The equivalent `http://localhost:8765/ui` address is also supported; browser
origins must use the daemon's actual listening port.

For another loopback port, use `hive web --url http://127.0.0.1:9000`.
This command opens a page; it does not start the daemon or check its health.

## Open a project session

Create the session in the [TUI](tui.md) from your intended project directory,
then select it in the browser's session rail. The existing session retains its
original directory and downstream identity.

> **New-session working directory:** the browser currently uses `/` and has no
> project-directory picker. Use a TUI-created session for project work.

The rail is visible by default. “Hide sessions” and “Show sessions” change its
presentation without closing the conversation. Catalog refreshes preserve focus
and selection. Recently visited sessions appear first during the current run.

## Take control

Selecting a session observes it read-only. Typing in the prompt box requests
pilot control. Send becomes available only after ownership is confirmed and the
session is idle. A previous pilot becomes read-only after takeover.

## Choose a provider and model

For a fresh session, choose a **Hive backend**, then **Create session**. This
discovers the agent's configuration without sending a prompt. Choose its
**Provider / Model** before sending the first message. Provider names and model
choices come from the agent; the Hive backend selects the agent process or
server, not its model provider.

Existing sessions show the current configuration. Use **Take control to change
model** before editing a read-only session. Controls are unavailable during an
active turn or pending configuration change. Changes become visible to other
clients and are restored on attachment from the daemon's authoritative state.
Backends without advertised configuration show an unsupported-selection message.
The OpenCode server adapter currently requires configuring models in OpenCode.

Only the pilot can cancel or answer session-scoped permission/input requests.
Forms use labelled controls with validation errors. URL consent does not itself
mean external authentication completed.

## Reconnect and retry

WebSocket reconnect resumes observation of the original session. It does not
automatically resend prompts. If a prompt acknowledgement is uncertain, use the
explicit retry action, which retains the original prompt key.

If session creation lost its acknowledgement, select the original session from
the catalog before prompting again. Creating another could start a second agent.

Reload reconstructs conversation history from retained replay. When history is
missing, choose attach-without-history explicitly. This restores current activity,
not missing messages. Daemon restart can additionally require the recovery
acknowledgement button before new work. See [session recovery](../operations.md#restart-and-recovery).

## Local browser data

Pending uncertain prompts, including their text and stable keys, are stored in
browser localStorage. Recently visited session navigation is client-local.
Hive's daemon catalog remains authoritative; the browser is not a durable
transcript archive.

Close the page to detach. The downstream agent keeps running while the daemon
remains alive.

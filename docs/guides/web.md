# Browser client

Start the daemon, then run:

```sh
hive web
```

The browser opens <http://127.0.0.1:8765/ui>. Assets are embedded in
Hive and served by the daemon. No separate frontend process is needed.
The equivalent `http://localhost:8765/ui` address is also supported; browser
origins must use the daemon's actual listening port.

The interface uses Vue 3.5 and OpenVue 1.0.0 with system-following light and dark
themes. Source builds require Node 22.22.2+ and a frontend build before Cargo;
see [development](../development.md#client-checks). The binary needs no Node runtime.

For another loopback port, use `hive web --url http://127.0.0.1:9000`.
This command opens a page; it does not start the daemon or check its health.

## Open a project session

Create the session in the [TUI](tui.md) from your intended project directory,
then select it in the browser's session rail. The existing session retains its
original directory and downstream identity.

> **New-session working directory:** the browser currently uses `/` and has no
> project-directory picker. Use a TUI-created session for project work.

The desktop rail is visible by default. “Hide sessions” and “Show sessions” change its
presentation without closing the conversation. Catalog refreshes preserve focus
and selection. Recently visited sessions appear first during the current run.
Search filters by title, ID, backend or project directory. On narrow screens,
the **Sessions** button opens a keyboard-accessible drawer. **New session** opens
a fresh browser view without terminating the existing agent session.

## Take control

Selecting a session observes it read-only. Typing in the prompt box requests
pilot control. Send becomes available only after ownership is confirmed and the
session is idle. A previous pilot becomes read-only after takeover.

## Choose a provider and model

For a fresh session, choose a **Hive backend**, then **Create session**. This
discovers the agent's configuration without sending a prompt. Choose its
**Provider / Model** before sending the first message or between later turns. Provider names and model
choices come from the agent; the Hive backend selects the agent process or
server, not its model provider.

Existing sessions show the current configuration. Use **Take control**
before editing a read-only session. Controls are unavailable during an
active turn or pending configuration change. Changes become visible to other
clients and are restored on attachment from the daemon's authoritative state.
Backends without advertised configuration show an unsupported-selection message.
The OpenCode server adapter currently requires configuring models in OpenCode.

Other advertised settings appear alongside Provider / Model with their agent's
labels. These can also be changed throughout an idle session. OpenCode exposes
Session Mode, Claude exposes permission mode, current Codex adapters expose both
approval and collaboration modes, and Pi exposes thinking level. An empty
catalog is disabled. A failed change displays an error and retains the confirmed
selection; agent-reported changes update all attached clients. Reconnect does
not resend an uncertain settings change.

Current mode is independent of configured defaults and displayed plans. See
[session configuration compatibility](../compatibility.md#session-modes-and-agent-settings)
for adapter versions, protocol mapping, and notification limitations.

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

## Conversation and activity

User messages, agent messages and session notices have distinct presentation.
Tool output and the activity summary expand independently. Content is rendered
as text, preserving whitespace without interpreting agent-provided HTML. Usage
distinguishes actual costs, estimates and unknown values.

Streaming follows the latest content while you are near the bottom. Scroll up
to read earlier messages without being pulled away; **Jump to latest** resumes
following. Updates preserve composer drafts, interaction fields and open tools.
The composer explains why sending is unavailable. Connection and pilot/read-only
indicators remain separate from session activity.

Spinners mark executing tool calls, background tasks, plan entries, live thinking,
and foreground work. Queued work and requests waiting for your input remain
static. Asynchronous actions show progress while pending; **Stopping** waits for
the agent to confirm idle. Background tasks can keep spinning after foreground
work ends. Disconnected or ended sessions do not animate stale activity, and
reduced-motion preferences replace animation with static indicators. Restored
thinking blocks stay static until new live thinking arrives for that block.

The initially expanded Activity panel includes **Plans**, separate from background
tasks. Plans show their IDs and the agent's ordered descriptions, statuses, and
priorities. “Supported · not yet observed” means Hive handles plans but this agent
has not emitted one; it is not an unsupported-agent claim. Unknown content types
show a generic text view, and empty plans are explicitly labelled. Plans remain
visible after idle and reconnect, even when their original events have expired.
After daemon restart, recovery depends on whether the agent replays its plans.
See the [native plan contract](../compatibility.md#native-acpv2-plans).

## Browser smoke checklist

Build with `mise run hive:build`, then start the daemon using the deterministic
[mock setup](../getting-started.md#try-without-a-model) or a configured local
agent. Open `/ui` from the daemon, not a frontend development server. The
automated adapter/component tests cover protocol and rendered interactions;
use this checklist for real browser layout, transport and accessibility checks.

1. Check the network panel: HTML, `/ui/app.js`, `/ui/style.css` and `/ui/icon.svg`
   load successfully, without external assets or browser console errors.
2. With an empty catalog, verify creation guidance. Stop/restart the daemon and
   verify visible connection errors/reconnect. Check list/backend retry actions
   when their endpoints fail.
3. Choose a backend, create a session, select an advertised grouped model, send
   a prompt, inspect streamed text/tool activity/usage, then stop active work.
   Model changes and send must be unavailable during pending operations.
4. Open a TUI-created session and a `?session=` deep link. Confirm the original
   project and retained transcript. Switch rapidly and search the session rail;
   no replacement sessions should be created.
5. In two tabs, take control in one, then the other. Verify the old pilot becomes
   read-only, stale actions do not succeed, and typing requests control explicitly.
6. Exercise permission choices, concurrent requests, cancellation, typed form
   validation and a request-scoped form during creation. Streaming must preserve
   unfinished fields. A withdrawn request must not accept an old answer.
7. For a URL request, verify safe external navigation and consent separately
   from the agent's eventual completion notification.
8. Disconnect during a prompt, reconnect to the original session, and confirm no
   automatic resend or pilot reclamation. Explicit retry retains the original
   content/key. Check explicit recovery acknowledgement after a daemon restart
   and attach-without-history when the replay window expires.
9. Scroll up during streaming, expand tools and type a draft. Confirm position,
   expanded state and focus persist; jump-to-latest resumes following.
10. Test desktop and 375 px mobile widths, long messages/tool output/model names,
    both OS color schemes and reduced motion. Tab through labelled controls,
    use the skip link and keyboard model choices, close the mobile drawer with
    Escape and verify focus returns. Status must be understandable without color.

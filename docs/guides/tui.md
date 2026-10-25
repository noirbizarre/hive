# Terminal client

Run `hive` or `hive tui` after starting the daemon. Start it in your project
directory: new sessions use the terminal's current working directory.

```sh
hive tui --agent opencode
```

`--agent` preselects a profile in the new-session picker. To attach to a known
session, use `hive tui --session hive-…`.

## Session navigation

The TUI opens with the session overview visible. It lists all directories by
default and hides after you open a session. Summaries refresh from the daemon,
so the same sessions are available in the Web UI and other ACP clients.

| Key | Action |
| --- | --- |
| `Ctrl-N` | Open the profile picker to create a session |
| Arrows, Enter | Select a profile or session |
| `Tab` | Show/hide the session overview while chatting |
| `a` in overview | Toggle all directories / current directory |
| Enter in overview | Attach as pilot |
| `o` in overview | Attach as observer |
| `Ctrl-B` | Return to the previously visited session |
| `Ctrl-T` | Take pilot control |
| `Ctrl-P` | Open/close the provider/model configuration picker |
| `Ctrl-H` | Relinquish control while observing |
| Enter in composer | Send the typed prompt |
| Page Up / Page Down | Scroll conversation history |
| Home / End | Jump to the oldest message / resume following live output |
| Mouse wheel over conversation | Scroll history |
| Click a tool summary | Expand/collapse formatted tool details |
| `Ctrl-Up` / `Ctrl-Down` | Select the previous/next tool call |
| `Ctrl-O` | Expand/collapse the selected tool (latest visible tool by default) |
| `Ctrl-C` | Cancel foreground work |
| `Ctrl-R` | Accept live-only attachment after a history gap |
| `Ctrl-Y` | Acknowledge interrupted work and take control |
| Esc | Close the model picker, or quit outside dialogs |
| `Ctrl-Q` | Quit without closing the session (outside agent questions) |

Recovery acknowledgement requires checking the downstream state first. See
[sessions and handoff](sessions.md) for what these operations guarantee.

## Choose a provider and model

Create a session with `Ctrl-N`, then press `Ctrl-P` before sending a prompt.
The modal lists the agent's advertised configuration choices, grouped by
provider when available; `*` marks current values. Type to fuzzy-search provider
and model names. Use arrows or Page Up/Down to navigate and Enter to apply a
choice. Successful changes close the modal; errors keep the search open for
retry. Esc or `Ctrl-P` closes it without changing your chat draft. Selection and
scroll position stay stable while the session catalog refreshes. Current choices
also appear under the conversation heading when space permits.

The backend profile chooses the agent process or server; provider/model
selection configures that agent. Take control with `Ctrl-T` before editing an
observed session, and wait for foreground work to finish. Unsupported backends
display an explanation. Model configuration for the OpenCode server adapter
currently stays in OpenCode itself.

## Answer agent questions

Permissions present numbered options. Select the intended option explicitly.

Forms edit individual fields rather than a comma-separated command:

- Tab and Shift-Tab move between fields.
- Arrow keys choose booleans and single-choice values.
- Arrows and Space toggle multiselect choices.
- `Ctrl-Space` includes an empty optional string or array.
- Enter submits; Esc declines. Invalid values stay editable.
- Page Up/Down inspect long question context.

Array input also accepts `|`-separated values or a JSON string array.
URL questions display an HTTP(S) link to open outside the TUI. Consenting to
continue is separate from the agent reporting external completion.

## Connection options

```sh
hive tui --address 127.0.0.1:9000 --transport http
```

The address is a host and port, not a URL. WebSocket (`ws`) is the default
transport; `http` selects HTTP/SSE. `HIVE_URL` and `HIVE_LISTEN` do not configure
the TUI. See [CLI reference](../reference/cli.md).

## Display

The transcript follows your terminal's foreground/background and ANSI colors.
Session details collapse in narrow terminals. Unknown activity remains unknown,
and an attention marker reflects only questions visible to this client.

Messages render Markdown headings, emphasis, lists, links, tables, and code
blocks. Tool calls start collapsed as a single line with the tool name (or
category), status, and a short action summary. Click that line to inspect input,
output, and file changes; text output is decoded and formatted instead of showing
the serialized protocol event. You can also select tools with `Ctrl-Up` /
`Ctrl-Down` and toggle details with `Ctrl-O`.

The session sidebar shows **Plans** separately from tools and background tasks,
with plan IDs and ordered descriptions, priorities, and statuses. Known Hive
support and whether the agent has emitted a plan are shown separately. “Not yet
observed” does not mean unsupported. Empty plans and unfamiliar content types are
explicitly labelled; unfamiliar content is displayed generically. Reconnect
restores current plans independently of the bounded event history. After daemon
restart, only plans replayed by the agent can be restored. See the
[native plan contract](../acp-compatibility.md#native-acpv2-plans).

Page Up/Down and the mouse wheel browse the retained conversation history. Home
jumps to the beginning; End returns to the latest output and resumes automatic
following. New output does not pull you away while reading older messages, and
each session retains its own scroll position and expanded tools. The footer
shows **History** while automatic following is paused. With an agent question or
model picker open, scrolling controls that dialog instead of the conversation.
Hold Shift when selecting terminal text if your terminal reserves mouse input
for the application.

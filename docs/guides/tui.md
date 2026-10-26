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
| `Ctrl-G` | Open/close the other session settings picker |
| `Ctrl-H` | Relinquish control while observing |
| Enter in composer | Send the typed prompt |
| `Alt-Enter` | Insert a newline without sending |
| Left / Right, Up / Down in composer | Move by character or wrapped row |
| Shift + arrows | Extend the prompt selection |
| `Ctrl-A` | Select the entire prompt |
| Backspace / Delete | Delete the selection or previous/next character |
| `Alt-C` / `Alt-V` / `Alt-X` | Copy selection / paste / cut prompt selection |
| `Ctrl-X`, then `Ctrl-E` | Open the prompt in an external editor |
| Page Up / Page Down | Scroll conversation history |
| Home / End | Jump to the oldest message / resume following live output |
| Mouse wheel over conversation | Scroll history |
| Click a tool summary | Expand/collapse formatted tool details |
| Drag over conversation text | Select text for copying |
| `Ctrl-Up` / `Ctrl-Down` | Select the previous/next tool call |
| `Ctrl-O` | Expand/collapse the selected tool (latest visible tool by default) |
| `Ctrl-C` | Cancel foreground work |
| `Ctrl-R` | Accept live-only attachment after a history gap |
| `Ctrl-Y` | Acknowledge interrupted work and take control |
| Esc | Close a configuration picker, or quit outside dialogs |
| `Ctrl-Q` | Quit without closing the session (outside agent questions) |

Recovery acknowledgement requires checking the downstream state first. See
[sessions and handoff](sessions.md) for what these operations guarantee.

## Edit and paste prompts

The composer shows the cursor and scrolls to keep it visible while editing long
or multiline prompts. Arrow movement and deletion respect Unicode characters,
including combining accents and emoji sequences. Each session retains its draft,
cursor, and prompt selection. Answering an agent question preserves the chat
draft separately from the answer.

Use `Alt-Enter` to add a newline and Enter to send. Bracketed paste and `Alt-V`
insert multiline text as one edit: pasted newlines never send a prompt or
confirm an agent question. Pasting into a form text field or configuration search
replaces newlines with spaces; permission choices do not accept pasted input.
Pastes and external-editor imports are limited to 8 MiB per operation.

Drag over the conversation to select text, or click in the composer and use
Shift-arrows or `Ctrl-A`. `Alt-C` copies the most recently focused selection;
`Alt-X` cuts only a prompt selection. Copies use rendered plain text, preserve
logical line breaks and code indentation, and omit soft-wrap newlines and screen
padding. Expand a tool before selecting its details. Dragging on a tool summary
selects text; clicking and releasing toggles it. Selecting history pauses live
following; End resumes it. Reflow preserves selections, but a rewrite of selected
content clears the selection so copying cannot silently use stale text.

`Ctrl-Shift-C`, `Ctrl-Shift-V`, and `Ctrl-Shift-X` also work when the terminal
delivers those distinct key combinations. Some terminals intercept them or send
the same key as plain Control; use the Alt bindings in that case. `Ctrl-C`
continues to cancel foreground work. Terminal-native paste also works through
bracketed paste; hold Shift to use terminal-native mouse selection where needed.

Clipboard actions use the system clipboard of the machine running Hive. Over
SSH this may be the remote machine's clipboard. If no desktop clipboard service
is available, Hive reports the failure and leaves the draft intact; terminal
copy/paste remains available.

## Use an external editor

Press `Ctrl-X`, then `Ctrl-E` to edit the current prompt in a temporary Markdown
file. Hive chooses the first nonempty value of `$VISUAL`, then `$EDITOR`, and
falls back to `vi`. For example:

```sh
EDITOR='code --wait' hive tui
```

The editor command accepts quoted executable paths and arguments. It runs
directly, without shell expansion or pipelines; use a wrapper executable for
shell logic. GUI editors need their wait option so Hive knows when editing has
finished. While the editor owns the terminal, Hive continues receiving agent
updates and queues incoming questions.

A successful exit loads the edited file as an unsent draft, including an empty
file. Enter still sends the prompt. An editor launch failure, nonzero exit, or
invalid UTF-8 preserves the original draft and reports an error. Hive restores
the terminal and cursor before accepting input again.

## Choose a provider and model

Create a session with `Ctrl-N`, then press `Ctrl-P` to choose a model, before
the first prompt or between later turns.
The modal lists the agent's advertised model choices, grouped by
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

## Change session settings

Press `Ctrl-G` for **Session settings**. This separate searchable picker contains
the agent's other advertised selections: for example OpenCode's Session Mode,
Claude's permission mode, Codex's collaboration mode, or Pi's thinking level.
Use Enter to apply, and Esc or `Ctrl-G` to close. Model selection keeps its
dedicated `Ctrl-P` picker.

Settings remain changeable throughout the session while you control an idle
agent. The heading shows confirmed current selections, also for observers.
Changes reported by the agent or made through another Hive client refresh these
values. Failed changes keep the picker open and do not claim a successful
selection. Reconnect uses the current snapshot without resending an uncertain
change; restart rediscovers settings from the original agent session.

Mode is agent-dependent: a selected Plan mode is separate from a plan in the
transcript or an agent's configured default. Unsupported or empty catalogs
cannot submit changes. See [session configuration compatibility](../acp-compatibility.md#session-modes-and-agent-settings).

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

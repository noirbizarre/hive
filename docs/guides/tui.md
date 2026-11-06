# Terminal client

Run `hive` or `hive tui` after starting the daemon. Start it in your project
directory: cwd-only sessions use the terminal's current working directory.
An applicable profile workspace policy instead provisions a checkout and uses
its effective directory.

```sh
hive tui --agent opencode
```

`--agent` preselects an agent in the new-session picker. Use
`hive tui --profile personal` to choose a named execution setup. In the picker,
press `p` to cycle execution profiles and implicit daemon selection; agent choices
follow their membership. Implicit selection follows the daemon's current default.
The **Profile default** entry lets the daemon resolve configured defaults and
reports ambiguous selection rather than choosing the first member.
To attach to a known
session, use `hive tui --session hive-…`.

## Session navigation

The TUI opens with the session overview visible. It lists all directories by
default and hides after you open a session. Summaries refresh from the daemon,
so the same sessions are available in the Web UI and other ACP clients.

| Key | Action |
| --- | --- |
| `Ctrl-N` | Open the agent picker to create a session |
| `p` in creation picker | Cycle named execution profiles |
| Arrows, Enter | Select an agent or session |
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
| `Alt-Backspace` | Remove the last image attachment |
| `Alt-R` in an empty composer | Restore rejected prompt content retained behind a newer draft |
| `Ctrl-X`, then `Ctrl-E` | Open the prompt in an external editor |
| Page Up / Page Down | Scroll conversation history |
| Home / End | Jump to the oldest message / resume following live output |
| Mouse wheel over conversation | Scroll history |
| Click a tool summary | Expand/collapse formatted tool details |
| Drag over conversation text | Select text for copying |
| Click an image or its caption | Select the image for copying with `Alt-C` |
| `Ctrl-Up` / `Ctrl-Down` | Select the previous/next tool call |
| `Ctrl-O` | Expand/collapse the selected tool (latest visible tool by default) |
| `Ctrl-C` | Cancel foreground work |
| `Ctrl-R` | Accept live-only attachment after a history gap |
| `Ctrl-Y` | Acknowledge interrupted work and take control |
| `Ctrl-L` | Explicitly retrieve a pending prompt receipt with its original key |
| Esc | Close a configuration picker, or quit outside dialogs |
| `Ctrl-Q` | Quit without closing the session (outside agent questions) |

Recovery acknowledgement requires checking the downstream state first. See
[sessions and handoff](sessions.md) for what these operations guarantee.
Reconnect observes the original session without resending pending prompts; use
`Ctrl-T` to take control. `Ctrl-Y` does not stop running work or skip missing
history. Receipt lookup never dispatches reserved work a second time.

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

Copy and cut prefer the terminal's clipboard through OSC 52 when write support
is detected or explicitly enabled. Over SSH this reaches the clipboard of the
machine running your terminal. When write support is unknown or disabled, Hive
uses the native clipboard of the machine running Hive, which may be remote.
`Alt-V` reads the terminal clipboard through OSC 5522 when rich reads are explicitly
enabled; otherwise it reads the native clipboard. Terminal-native text paste remains
available through bracketed paste.

OSC 52 copies are limited to 64 KiB of UTF-8 text before Base64 encoding (up to
87,384 encoded payload bytes). Larger selections are rejected without truncation;
select less text or disable OSC 52 writes to use the native clipboard. A terminal
may impose a smaller limit or silently deny a write: a successful copy status
means Hive sent the sequence, not that the terminal accepted it. Output failures
and unavailable native clipboards are reported without deleting a cut selection.
Failed OSC writes do not silently switch to a different machine's clipboard.

### Paste and copy images

On Unix, Hive negotiates private mode 5522 with the terminal. Supporting terminals
send a rich clipboard offer when you trigger their normal paste action. Hive requests
PNG, JPEG, or WebP in that preference order, falling back to plain text. The terminal
may include a one-time permission token for this user-triggered read. Disable these
offers with `--terminal-osc5522-read off`. Unsupported terminals keep bracketed paste.

To read the terminal clipboard explicitly with `Alt-V` or `Ctrl-Shift-V`, opt in:

```sh
hive tui --terminal-osc5522-read on
```

An image becomes an unsent attachment, with its format and dimensions shown in the
composer heading. Enter sends text plus attachments, or an image alone. `Alt-Backspace`
removes the last attachment. Attachments survive session switching, agent questions,
and external editing; the external editor changes only text. Images cannot be pasted
into question fields or configuration searches. Clipboard HTML, SVG, file paths,
and URLs are not imported as images or fetched automatically.

Image input requires an ACPv2 agent advertising image-prompt support. The v1 and
OpenCode server adapters currently accept text only. Unsupported sessions retain
the draft and explain how to proceed. A rejected prompt never overwrites newer
unsent work: clear the composer and use `Alt-R` to restore the retained rejection.
Receipt lookup retains the original image data and intent key.

Click an agent image or its fallback caption, then use `Alt-C` to copy its image
data. Dragged text selections still copy captions as text. Rich copies prefer
OSC 5522 when negotiated or enabled, and claim success only after acknowledgement;
otherwise Hive uses the native image clipboard. A denied terminal request is
reported without silently accessing another clipboard. On non-Unix platforms,
native image clipboard operations remain available, but OSC 5522 is unavailable.

Rich input is bounded to four images and 8 MiB of compressed image data per draft,
16,384 pixels per dimension, and 40 million pixels per image. Images are fully
decoded for validation before attachment. The complete serialized outgoing prompt
is limited to 15 MiB, including Base64 and JSON escaping. Malformed transfers,
unsupported data, permission denial, and transfer timeouts leave drafts unchanged.
If a terminal sends an unterminated control reply, Hive discards its remaining bytes
and asks you to press Escape once to restore keyboard input before retrying.

OSC 5522 image reads enforce bounds before decoding. The native clipboard backend
decodes images before returning them to Hive, so its initial allocation is outside
Hive's decoder limits; Hive validates the returned dimensions and encoded size
before attaching the image.

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
cannot submit changes. See [session configuration compatibility](../compatibility.md#session-modes-and-agent-settings).

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

### Terminal capabilities

The Sessions heading shows a compact **Terminal caps** diagnostic, initialized
when the local TUI starts and shared by TUI components. Rich clipboard negotiation
can subsequently establish write support, and program status negotiation can
establish OSC 7501 support. `graphics` means Kitty
graphics, `OSC52` is the text clipboard protocol, `OSC5522` is the rich clipboard
protocol, and `OSC7501` is program status reporting. Clipboard entries distinguish
`w` (write) and `r` (read).

- `+`: permitted to attempt the operation based on detection or configuration.
- `?`: unknown; the operation falls back without using the protocol.
- `-`: disabled or unsupported.
- `*` following a state: explicitly overridden, rather than detected.
- **unavailable**: stdin or stdout is not a TTY, or `TERM=dumb`; overrides cannot
  enable optional terminal operations on that transport.

The line clips in compact layouts. It reports permission to attempt an operation,
not guaranteed terminal acceptance. ACP message images use the PNG Kitty backend.
OSC 52 writes and OSC 5522 image operations use the shared clipboard service.
Programmatic OSC 52 reads remain follow-up work.
Existing text rendering, native clipboard operations, and terminal-native paste
remain usable without any positive capability decision.

Environment-based detection is deliberately conservative. On Unix, Hive also queries
private mode 5522 to negotiate user-triggered rich paste (unless rich reads are off):

| Evidence | Automatic decision |
| --- | --- |
| Direct `TERM=xterm-kitty` | Kitty graphics and OSC 52 writes supported |
| `TMUX`, `STY`, or `ZELLIJ` present (even empty), or `TERM` starting with `screen`/`tmux` | All optional protocol support unknown |
| Missing, empty, generic, or unrecognized terminal identity | All optional protocol support unknown |
| Supporting response to the mode 5522 query | Native rich paste and OSC 5522 writes supported |
| Supporting response to the OSC 7501 query (Unix) | Program status supported, respecting advertised states and kinds |
| No OSC 7501 response within 500 ms (Unix) | Program status unsupported |
| Clipboard reads on any terminal | Unknown without explicit read opt-in |

Environment variables are hints, not negotiation: SSH may omit them or forward
stale values, multiplexers may block protocols, and terminal versions and user
settings vary. Kitty-specific incidental variables do not establish support for
newer protocols. Overrides assert support but neither implement multiplexer
passthrough nor guarantee that the terminal will accept an operation.

#### Agent program status

Hive reports the attached session's foreground agent state through
[OSC 7501 Program Status](https://www.superlogical.com/rex/docs/build/program-status).
The terminal chooses how to present it. Running work reports `working`, and
action-required work reports `blocked`; session permissions and questions include
the corresponding blocked kind. Explicit successful completion reports `done`,
failures report `error`, and cancellation or idle without a known outcome reports
`idle`. Background tools/tasks do not reopen completed foreground work, and no
percentage is inferred from their counts.

Reports include the session identifier and a short description. Switching sessions
replaces the report; merely highlighting another session does not. Done/error
persists until another authoritative state or session switch, rather than ordinary
typing; freshly attaching to an idle session does not resurrect old outcomes.
Disconnects, unknown/ended state, history gaps, editor handoff, and exit clear only
Hive's own record. Returning from the editor restores the current state.

In `auto`, Unix clients query once without blocking input. Unsupported terminals
continue normally. Terminals advertising only some states receive conservative
fallbacks: blocked to working, and done/error to idle with outcome context, where
supported; otherwise Hive clears its record. Unsupported blocked kinds are omitted.
`--terminal-osc7501 on` asserts baseline write support without negotiation; `off`
disables queries and reports. Non-Unix clients do not negotiate automatically but
can use write-only reporting with `on` on an available terminal transport.

Clipboard write support never grants arbitrary read permission. Rich read `on`
opts into explicit OSC 5522 reads. Negotiated native paste handles user-triggered
offers without enabling background reads. OSC 52 reads remain unimplemented even
with `on`; use OSC 5522 reads or terminal-native paste instead. See
[capability overrides](../reference/cli.md#terminal-capability-overrides) for all
CLI options and environment variables. For example:

```sh
hive tui --terminal-osc52-write on --terminal-osc52-read off
```

### Kitty image backend

ACP conversation messages preserve images in their original position among text
blocks, including multiple images in one message. PNG, JPEG, and WebP are supported;
JPEG and WebP are converted locally to PNG for terminal transport. Animated WebP
uses its first frame as a static image. Pixel preparation happens only when an
image is fully visible with graphics enabled, and is cached across redraws and
text streaming. Image URIs are displayed as metadata and are never fetched.

Message captions include an image number, original MIME type, and dimensions when
available. Unsupported formats, locally detected malformed data, or exceeded limits produce an
explanatory caption. Copying transcript text includes captions, without image data
or blank pixel rows. Disabled or unknown graphics capability keeps captions only.

The TUI image service accepts PNG data and supports inline images and bounded
thumbnails. Kitty preserves the image aspect ratio inside the allocated cell
rectangle. Inline bounds default to 80 columns by 20 rows; thumbnails default to
20 columns by 6 rows. Both are constrained by the caller's available viewport.
The same upload can be displayed at multiple sizes.

Captions include the image type, dimensions, and a caller-provided label. Unknown
or disabled graphics capability uses text only, with no Kitty image escapes.
Unsupported formats or rejected sources can also use a metadata-only fallback.
The service checks PNG headers and resource bounds (16 MiB input, dimensions up
to 16,384 pixels, and 40 million pixels total); the terminal performs PNG decoding.
Quiet transfers do not query whether the terminal accepted an image, so captions
remain visible even when graphics are attempted.

ACP base64 decoding is bounded before allocation. JPEG/WebP decoding also limits
dimensions and allocation; the 16 MiB transfer limit applies to both input bytes
and converted PNG output. Transcript reservations shrink to fit the viewport.
Partially visible image reservations show a scrolling hint instead of rescaling
the image over neighboring text; scroll until the caption and image fit fully
inside the conversation viewport to display the pixels.

The renderer replaces placements during redraws, reuses uploads during ordinary
redraws, and frees images when they leave the view. Resizing or clearing the
screen invalidates uploads. Images are suppressed behind pickers and cleaned up
before editor handoff and terminal restoration. Only Hive-owned image IDs are
deleted.

Developers can inspect an ACP conversation scene in a Kitty-capable terminal:

```sh
mise exec -- cargo test -p hive-tui \
  tests::inline_images_and_thumbnails_can_be_inspected_in_a_terminal \
  -- --ignored --exact --nocapture
```

Resize the terminal to check reflow. Use Up/Down and Home/End to scroll the ACP
scene; press `a` to switch to the synthetic inline/thumbnail scene.
Press `h` to remove/restore content, `o` to toggle an overlay, `c` to clear/rebuild
the screen, and `q` to exit. Capability
overrides also apply to this fixture, for example
`HIVE_TERMINAL_KITTY_GRAPHICS=off` to inspect fallback. The automated
`python3 crates/hive-tui/tests/terminal.py` fixture checks image cleanup across
editor handoff using a controlling PTY.

### Transcript and activity

The transcript follows your terminal's foreground/background and ANSI colors.
Session details collapse in narrow terminals. Unknown activity remains unknown,
and an attention marker reflects only questions visible to this client.

Spinners mark executing tools, tasks, plan entries, live thinking, foreground
work, and pending asynchronous actions. Queued work and requests awaiting your
input stay static. Session and control actions keep the interface responsive
while waiting; cancellation shows **Stopping** until the agent confirms idle.
Background tasks can continue after foreground work ends. Disconnected or ended
sessions do not animate stale activity. Restored thinking stays static until
new live thinking arrives for that block. Animation preserves transcript
selection, copied text, scroll position, and expanded tool details.

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
[native plan contract](../compatibility.md#native-acpv2-plans).

Page Up/Down and the mouse wheel browse the retained conversation history. Home
jumps to the beginning; End returns to the latest output and resumes automatic
following. New output does not pull you away while reading older messages, and
each session retains its own scroll position and expanded tools. The footer
shows **History** while automatic following is paused. With an agent question or
model picker open, scrolling controls that dialog instead of the conversation.
Hold Shift when selecting terminal text if your terminal reserves mouse input
for the application.

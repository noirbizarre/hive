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
| `Ctrl-H` | Relinquish control while observing |
| Enter in composer | Send the typed prompt |
| `Ctrl-C` | Cancel foreground work |
| `Ctrl-R` | Accept live-only attachment after a history gap |
| `Ctrl-Y` | Acknowledge interrupted work and take control |
| Esc / `Ctrl-Q` | Quit without closing the session |

Recovery acknowledgement requires checking the downstream state first. See
[sessions and handoff](sessions.md) for what these operations guarantee.

If quitting restores the screen but leaves the process waiting, press Enter
once. See the [terminal-reader workaround](../troubleshooting.md#the-tui-leaves-its-screen-but-does-not-return-to-the-shell).

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

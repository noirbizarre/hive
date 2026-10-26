# CLI reference

`hive` without a subcommand opens the TUI. It does not start the daemon.
Use `hive <command> --help` for the installed executable's exact help.

## Commands

| Command | Purpose | Options |
| --- | --- | --- |
| `hive` / `hive tui` | Terminal client | `--address`, `--transport`, `--agent`, `--session` |
| `hive daemon` | Foreground daemon | `--listen`, `--config` |
| `hive daemon status` | Print daemon status response | `--url` |
| `hive daemon reload` | Reload the original config for new sessions | `--url` |
| `hive web` | Open embedded browser client | `--url` |
| `hive doctor [AGENT]...` | Diagnose all or selected profiles and daemon setup | `--config`, `--listen`, `--json` |
| `hive completions <shell>` | Write shell completion script to stdout | Shell argument |
| `hive serve` | Legacy alias for daemon startup | `--listen`, `--config` |
| `hive status` | Legacy alias for daemon status | `--url` |

The CLI has no background daemon management, config initialization, agent login,
prompt submission, native adoption, or session deletion subcommands.

## TUI options

| Option | Default | Meaning |
| --- | --- | --- |
| `--address` | `127.0.0.1:8765` | Daemon host and port, without URL scheme |
| `--transport` | `ws` | `ws` or `http` (HTTP/SSE) |
| `--agent` | Unset | Preselect a profile in the creation picker |
| `--session` | Unset | Attach to a known Hive session ID |

These work on both `hive` and `hive tui`. The TUI does not derive its address
from `HIVE_URL` or `HIVE_LISTEN`.

## Daemon and doctor

`--listen` is a socket address, defaulting to `127.0.0.1:8765`. Non-loopback
daemon listeners are refused. `--config` selects a TOML file rather than merging
with the default. See [configuration](../configuration.md).

Doctor checks every profile unless IDs are supplied. `--json` prints one report
object. Errors exit 1; warnings alone exit 0. It probes initialization without
prompting and does not create daemon lock/state files.

## Web, status, and reload URLs

Their daemon base URL is selected in this order:

1. Explicit `--url`.
2. `HIVE_URL`.
3. `http://$HIVE_LISTEN`.
4. `http://127.0.0.1:8765`.

For status, place `--url` on the status subcommand:

```sh
hive daemon status --url http://127.0.0.1:9000
hive daemon reload --url http://127.0.0.1:9000
```

The parent daemon `--listen` option does not set the status or reload URL. Web appends
`/ui` to its base URL.

Reload accepts no configuration-path option: the running daemon retains its
startup source. It prints JSON on success and a diagnostic with nonzero exit
status on failure. Unix SIGHUP invokes the same operation. See
[operations](../operations.md#reloading-configuration).

## Environment variables

| Variable | Used by | Purpose |
| --- | --- | --- |
| `HIVE_CONFIG` | Daemon, doctor, serve | Configuration file |
| `HIVE_DEFAULT_AGENT` | Configuration loading | Override default profile ID |
| `HIVE_LISTEN` | Daemon, doctor, serve | Listen address; URL fallback for status/web/reload |
| `HIVE_URL` | Status, web, reload | Daemon base URL |
| `RUST_LOG` | Daemon tracing | Log filter |
| `VISUAL` | TUI | Preferred external prompt editor, executable and arguments |
| `EDITOR` | TUI | Prompt editor when `VISUAL` is empty or unset |
| `XDG_CONFIG_HOME` | Configuration lookup | Configuration base directory |
| `XDG_RUNTIME_DIR` | Instance lock | Runtime base directory |
| `XDG_STATE_HOME` | Catalog | Durable state base directory |
| `HOME` | Path fallbacks | Fallback base directory |

Explicit CLI options override their environment equivalents. Agent-specific
credentials and profile `password_env` variables belong to the daemon environment.

The TUI's `Ctrl-X`, `Ctrl-E` editor shortcut uses `vi` if both editor variables
are empty or unset. See [external editing](../guides/tui.md#use-an-external-editor).

## Shell completions

```sh
hive completions bash
hive completions zsh
```

Redirect stdout to the appropriate completion file for your shell, creating its
parent directory first. `hive completions --help` lists supported shell names.

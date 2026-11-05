# CLI reference

`hive` without a subcommand opens the TUI. It does not start the daemon.
Use `hive <command> --help` for the installed executable's exact help.

## Commands

| Command | Purpose | Options |
| --- | --- | --- |
| `hive` / `hive tui` | Terminal client | `--address`, `--transport`, `--agent`, `--session` |
| `hive daemon` | Foreground daemon | `--listen`, `--config`, `--allow-remote`, `--auth-token-env`, `--allowed-origin` |
| `hive daemon status` | Print daemon status response | `--url` |
| `hive daemon reload` | Reload the original config for new sessions | `--url` |
| `hive project <command>` | Manage project definitions and node-local overrides | `--url` |
| `hive repository <command>` | List, inspect and fetch node caches | `--url` |
| `hive workspace <command>` | Provision, inspect and remove task workspaces | `--url` |
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
| `--transport` | `ws` | `ws`, `http` (HTTP/SSE), `wss`, or `https` |
| `--agent` | Unset | Preselect a profile in the creation picker |
| `--session` | Unset | Attach to a known Hive session ID |
| `--cwd` | Unset | Explicit existing directory, bypassing a profile workspace policy |
| `--project` | Profile default | Project override for workspace creation |
| `--workspace` | Profile default | Named workspace policy |
| `--workspace-id` | Unset | Attach a ready workspace to a new session |
| `--base-ref` / `--branch` | Policy/task default | Base ref or fresh branch for managed creation |
| `--revision` | JJ policy/project default, then `trunk()` | Native JJ revision/change expression; conflicts with Git ref/branch selectors |

These work on both `hive` and `hive tui`. The TUI does not derive its address
from `HIVE_URL` or `HIVE_LISTEN`.

### Terminal capability overrides

These local client options accept `auto`, `on`, or `off`, defaulting to `auto`.
CLI values override environment values, including an explicit `auto` restoring
automatic detection. They work on both `hive` and `hive tui`.

| Option | Environment variable | Operation |
| --- | --- | --- |
| `--terminal-kitty-graphics` | `HIVE_TERMINAL_KITTY_GRAPHICS` | Kitty graphics (not keyboard support) |
| `--terminal-osc52-write` | `HIVE_TERMINAL_OSC52_WRITE` | Text clipboard writes |
| `--terminal-osc52-read` | `HIVE_TERMINAL_OSC52_READ` | Text clipboard read attempts |
| `--terminal-osc5522-write` | `HIVE_TERMINAL_OSC5522_WRITE` | Rich clipboard writes |
| `--terminal-osc5522-read` | `HIVE_TERMINAL_OSC5522_READ` | Rich clipboard read attempts |
| `--terminal-osc7501` | `HIVE_TERMINAL_OSC7501` | Program status reporting |

`on` asserts support for that operation; for reads it also explicitly opts into
attempting clipboard access. Read and write settings are independent. The
terminal may still deny access. `off` disables that terminal operation, and unknown
automatic support falls back without using the protocol. A non-TTY stdin or
stdout, or `TERM=dumb`, makes all six operations unavailable even with `on`.

```sh
hive tui --terminal-kitty-graphics off --terminal-osc52-write on
HIVE_TERMINAL_OSC52_WRITE=on hive --terminal-osc52-write auto
```

OSC 52 writes are preferred for copy/cut when permitted; otherwise Hive uses the
native clipboard. Terminal writes have a 64 KiB UTF-8 text limit and no acceptance
acknowledgement. Native clipboard operations remain independent of these options.
Programmatic OSC 52 reads are not implemented: the input backend cannot receive
clipboard replies, so even read `on` never sends a query. Paste uses the native
clipboard or terminal-native bracketed paste. Rich clipboard and program status
backends remain follow-up work.
See [terminal capabilities](../guides/tui.md#terminal-capabilities) for detection
limitations and the TUI diagnostic legend.

## Workspaces and repositories

Workspace creation flags conflict with `--session`; explicit `--cwd` conflicts
with managed-workspace selectors. These options apply when creating a session
from the profile picker, not when resuming one.

```sh
hive --agent coder --project hive --workspace task
hive workspace create --project hive --policy task
hive workspace create --project jj-project --policy jj-task --revision 'latest(main@origin)'
hive workspace list
hive workspace show <workspace-id>
hive workspace inspect <workspace-id>
hive workspace recovery
hive workspace sweep plan
hive workspace sweep show <operation-id>
hive workspace sweep apply <operation-id>
hive workspace operation <operation-id>
hive workspace reconcile <workspace-id>
hive workspace remove <workspace-id>
hive repository list
hive repository show <repository-id>
hive repository fetch <repository-id>
```

Creation/removal accept `--operation` for identical retries. Removal also accepts
`--discard-changes`; this overrides dirty-data protection only, never ownership
or active-use checks. See [workspace operations](../operations.md#repositories-and-task-workspaces).

`recovery` reports incomplete/orphan resources, session holds and retention
blockers without deleting anything. `sweep plan` freezes expired ephemeral
candidates and accepts repeated `--workspace <id>` selectors and `--operation`
for identical retries. Planning does not execute cleanup. `sweep apply` explicitly
applies/retries that plan with fresh safety checks and never discards changes.
Mixed results print as JSON and unresolved items exit nonzero. Persistent
workspaces require explicit `remove`; restart never resumes a saved sweep.

## Daemon and doctor

`--listen` is a socket address, defaulting to `127.0.0.1:8765`. Non-loopback
listeners require `--allow-remote`, independently of optional authentication.
`--auth-token-env NAME` names the environment variable holding the shared token;
configured missing/empty values fail closed. Repeat `--allowed-origin ORIGIN` for
exact browser origins. Doctor accepts the same three policy options; `serve`
accepts the daemon startup options. They require daemon restart to change.
`--config` selects a TOML file rather than merging with the default. See
[private-network access](../operations.md#private-network-access).

Doctor checks every profile unless IDs are supplied. `--json` prints one report
object. Errors exit 1; warnings alone exit 0. It probes initialization without
prompting and does not create daemon lock/state files.

## Web, status, and reload URLs

Their daemon base URL, also used by project commands, is selected in this order:

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
| `HIVE_ALLOW_REMOTE` | Daemon, doctor, serve | Explicit non-loopback bind opt-in |
| `HIVE_AUTH_TOKEN_ENV` | Daemon, doctor and all native clients | Name of the environment variable holding the optional bearer token |
| `HIVE_ALLOWED_ORIGINS` | Daemon, doctor, serve | Comma-separated allowed browser origins |
| `HIVE_DEFAULT_AGENT` | Configuration loading | Override default profile ID |
| `HIVE_LISTEN` | Daemon, doctor, serve | Listen address; URL fallback for status/web/reload/project |
| `HIVE_URL` | Status, web, reload, project | Daemon base URL |
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

## Projects

Project commands print JSON and use the same daemon URL precedence as status.
`--url` works anywhere after `hive project`. Definitions registered through the
CLI persist in the daemon's database, independently of TOML-owned definitions.

```sh
hive project register --name Hive --remote https://github.com/noirbizarre/hive.git \
  --default-ref main --preparation '[["mise","install"]]'
hive project register --name "JJ project" --scm jj \
  --remote https://example.com/team/project.git --default-revision 'trunk()'
hive project list
hive project show hive
hive project update hive --name "Hive renamed"
hive project remove hive
```

Registration derives a lowercase slug from `--name` unless `--id` is supplied.
If the slug is taken, it tries `-2`, `-3`, and so on; the chosen ID survives renames.
Only `--name` is required;
omit `--remote` for local-only projects. Update replaces all metadata: omitted
remote/ref fields are cleared and preparation defaults to `[]`. Registration and
update do not clone or execute preparation. Config-owned update/removal is rejected;
edit TOML and run `hive daemon reload` instead.
`--scm` defaults to `git` on registration and replacement. JJ uses
`--default-revision`; Git uses `--default-ref`. Workspace provisioning still requires
a compatible named policy; see [native JJ configuration](../configuration.md#native-jujutsu-workspaces).

Directory overrides require an explicit stable node ID; Hive does not allocate a
new node identity for each request:

```sh
hive project override set hive \
  node-22222222-2222-4222-8222-222222222222 /home/user/work/hive
hive project override show hive \
  node-22222222-2222-4222-8222-222222222222
hive project override remove hive \
  node-22222222-2222-4222-8222-222222222222
```

Use `--syntax windows` for Windows absolute paths; the default is `posix`.
Removing a project only removes its definition, preserving all local directories,
overrides, repository instances, and workspaces.

## Shell completions

```sh
hive completions bash
hive completions zsh
```

Redirect stdout to the appropriate completion file for your shell, creating its
parent directory first. `hive completions --help` lists supported shell names.

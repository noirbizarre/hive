# Configuration

Hive loads one TOML file when the daemon starts. Profiles describe how to reach
agents. They do not start agents until a session needs them.

## File location and precedence

The file is selected in this order:

1. `--config /path/to/config.toml`.
2. `HIVE_CONFIG`.
3. `$XDG_CONFIG_HOME/hive/config.toml`.
4. `$HOME/.config/hive/config.toml` when `XDG_CONFIG_HOME` is unset.
5. `./hive/config.toml` when both variables are unset.

An absent default file produces an empty configuration. An absent explicitly
selected file is an error. Files are not merged. Unknown fields are rejected
rather than silently ignored.

Use the same file when checking and serving:

```sh
hive doctor --config /absolute/path/config.toml
hive daemon --config /absolute/path/config.toml
```

## Reloading changes

After editing, run `hive daemon reload`, or send SIGHUP to the daemon on Unix.
Hive rereads the original path, including files replaced atomically by an editor.
It validates the entire candidate before publishing it. Invalid or unreadable
files leave the previous configuration active; deleting even an implicit default
file causes reload to fail rather than clear the registry.

New defaults and profiles apply to new sessions. Existing sessions keep their
original agents, connections, credentials, and pricing, even when their profile
is removed. Open browser and TUI clients refresh their provider lists automatically.
Reload does not retry recovery of offline sessions.

An OpenCode profile ID remains associated with its existing sessions, including
saved offline sessions. Changing its server connection identity or adapter kind
is rejected with `hive::reload::profile_identity`; use a new profile ID for the
new server. Removing and re-adding the old ID does not erase that association.
Connection identity conservatively includes endpoint, headers, authentication
username, and password environment-variable name, so routing/authentication edits
can also require a new ID. Secrets are not stored as plaintext in the identity.
Older saved sessions establish their initial identity from the startup profile;
that pre-upgrade identity cannot be independently verified. If the startup profile
is absent, an unknown legacy binding reserves its ID; use a new ID rather than
reusing it for another server.

Reload uses the daemon's environment, including `HIVE_DEFAULT_AGENT`; exporting
variables in the calling shell does not update a running process. The original
file path is retained even if the reload caller sets another `HIVE_CONFIG`.
Listen address, state/runtime locations, and tracing settings require restart.

## Top-level settings

| Setting | Type | Default | Meaning |
| --- | --- | --- | --- |
| `default_agent` | String | Unset | Profile ID for session creation without a selector |
| `agents` | Table of profiles | Empty | Available downstream agents |

`HIVE_DEFAULT_AGENT` overrides `default_agent`. The selected ID must exist.
A daemon can start without a default, but clients must then select a profile.

Profile IDs contain only ASCII letters, digits, `-`, or `_`, and cannot be empty.
The ID in `[agents.opencode]` is `opencode`. Its optional `name` is only a display
label and is not a selector.

## Profile fields

| Field | Type | Default | Applies to |
| --- | --- | --- | --- |
| `name` | String | Profile ID for display | All profiles |
| `kind` | `"acp"` or `"opencode-server"` | `"acp"` | All profiles |
| `command` | String | Unset | ACP stdio executable |
| `args` | Array of strings | `[]` | ACP stdio arguments |
| `env` | String-to-string table | `{}` | ACP stdio environment overrides |
| `endpoint` | URL string | Unset | ACP network or OpenCode server |
| `headers` | String-to-string table | `{}` | Endpoint request headers |
| `versions` | Array of `"v1"` / `"v2"` | `["v2"]` | ACP version selection |
| `username` | String | `"opencode"` when using password auth | OpenCode server |
| `password_env` | String | Unset | Name of OpenCode password environment variable |
| `pricing` | Table | No price mapping | Usage estimates |

For ACP, choose either `command` or `endpoint`. A command profile cannot have
headers. An endpoint profile cannot have `command`, `args`, or `env`.
`username` and `password_env` are rejected on ACP profiles.

`versions` cannot be empty. Hive prefers v2 when both versions are declared.
An initialization failure does not trigger a v1 fallback. The OpenCode server
adapter uses REST/SSE rather than selecting an ACP wire version.

## ACP over stdio

```toml
default_agent = "opencode"

[agents.opencode]
name = "OpenCode ACP"
command = "opencode"
args = ["acp"]
versions = ["v1"]
env = { EXAMPLE_SETTING = "value" }
```

`command` is an executable, not a shell command line. Put each argument in its
own array element. Use an absolute executable path when launch location matters.
The process inherits the daemon environment with profile overrides applied.

Each session gets its own downstream connection and, for stdio, its own process.
Check the agent's actual entrypoint and supported version before selecting it.

## ACP over a network transport

```toml
[agents.network-agent]
endpoint = "http://127.0.0.1:9000"
versions = ["v2"]
headers = { Authorization = "Bearer replace-with-your-token" }
```

ACP endpoints accept `http://`, `https://`, `ws://`, and `wss://`. The endpoint
must serve ACP, not an unrelated REST API. See the [transport matrix](transports.md).

Header strings are literal. Hive does not expand `${TOKEN}` in configuration
strings. Keep configuration containing credentials private and omit it from
public bug reports.

## OpenCode server

```toml
[agents.opencode-server]
kind = "opencode-server"
endpoint = "http://127.0.0.1:4096"
username = "opencode"
password_env = "OPENCODE_SERVER_PASSWORD"
```

Start and authenticate the server separately. Hive does not launch it.
`password_env` names a variable in the **daemon's** environment, not a secret
value. If configured, that variable must be set and nonempty when connecting.

This profile cannot contain `command`, `args`, or `env`. Its endpoint must be
HTTP(S), must not embed credentials, and may use plain HTTP only on loopback.
These stricter endpoint checks are specific to the OpenCode adapter. They are
not a promise that all ACP endpoints enforce the same restrictions.

See [OpenCode integration](agents/opencode.md) for native-session adoption.

## Pricing

Usage tracking works without pricing. Estimates require the exact billed
provider and model identifiers from [models.dev](https://models.dev/).

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `provider` | String | Unset | Exact models.dev provider ID |
| `model` | String | Unset | Fixed catalog model ID |
| `models` | String-to-string table | `{}` | Agent selector ID → catalog model ID |
| `cached_input_included` | Boolean | `false` | Input token count includes both cache categories |

For a fixed model:

```toml
[agents.opencode.pricing]
provider = "anthropic"
model = "claude-sonnet-4-5"
cached_input_included = true
```

Use this only if those are the actual provider/model and cache semantics of
your agent. For selectable models, replace `model` with a mapping:

```toml
[agents.opencode.pricing]
provider = "anthropic"
models = { "agent-model-selector" = "claude-sonnet-4-5" }
```

A price mapping needs a provider and either a fixed model or selector mappings.
Hive does not infer billing identity from the profile name. Unknown mappings,
missing token categories, or catalog failures produce unknown estimates rather
than fabricated zero costs. If cache counts are reported without confirmed
inclusive-input semantics, ordinary input cost can remain unknown.

Provider-reported cumulative costs and estimates are distinct. Read the
[usage contract](acp-compatibility.md#token-usage-and-costs) for aggregation rules.

## Runtime locations

| Purpose | Preferred location | Fallback |
| --- | --- | --- |
| Configuration | `$XDG_CONFIG_HOME/hive/config.toml` | `$HOME/.config/hive/config.toml` |
| Instance lock | `$XDG_RUNTIME_DIR/hive/daemon.lock` | `$HOME/.local/state/hive/daemon.lock` |
| SQLite catalog | `$XDG_STATE_HOME/hive/sessions.sqlite3` | `$HOME/.local/state/hive/sessions.sqlite3` |

The lock fallback does not use `XDG_STATE_HOME`. Lock and database lookup fail
when neither their XDG variable nor `HOME` is available.

Hive creates private runtime/state directories. On Unix, existing directories
or files with unsafe permissions can prevent startup. See
[operations](operations.md#storage-and-backups) before copying or changing state.

## Listener and client settings

Listener addresses and client connection options are CLI/environment settings,
not TOML fields. The daemon defaults to `127.0.0.1:8765` and rejects non-loopback
listeners. For a different port:

```sh
hive daemon --listen 127.0.0.1:9000
hive daemon status --url http://127.0.0.1:9000
hive tui --address 127.0.0.1:9000
hive web --url http://127.0.0.1:9000
```

The daemon/doctor use `HIVE_LISTEN`. Web and status support `HIVE_URL`
and fall back to `HIVE_LISTEN`. The TUI uses its own `--address` and does not
read either variable. See the [CLI reference](reference/cli.md) for precedence.

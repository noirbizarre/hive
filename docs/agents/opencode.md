# OpenCode

Hive has two distinct OpenCode integrations. Choose based on whether you want
fresh Hive-managed ACP sessions or access to native OpenCode conversations.

The retained evidence concerns OpenCode 1.x. OpenCode 2 uses the same executable
name but is not covered by those probes. Check the installed major version and
entrypoint rather than treating all `opencode` installations as equivalent.

| Profile | Connection | Use it for |
| --- | --- | --- |
| `kind = "acp"` | Spawn `opencode acp` over stdio | New sessions through Hive |
| `kind = "opencode-server"` | Existing server over REST/SSE | Native discovery/adoption and server-backed sessions |

## ACP sessions

Install and authenticate OpenCode separately, then configure:

```toml
default_agent = "opencode"

[agents.opencode]
name = "OpenCode ACP"
command = "opencode"
args = ["acp"]
versions = ["v1"]
```

Run `hive doctor opencode`, start the daemon, and create a session in the TUI.
See [getting started](../getting-started.md) for the full sequence.

This launches a separate ACP process per Hive session. It is not native-session
adoption. Confirm the wire version after upgrades and keep initialization,
prompting, and recovery evidence separate in the
[Known agents matrix](../compatibility.md#known-agents).

### Mode and model selection

When OpenCode advertises ACP session configuration, choose **Provider / Model**
in the browser or press `Ctrl-P` in the TUI. Create the session first to discover
its choices; no inference is required for discovery. The selected model belongs
to OpenCode, independently of Hive's `opencode` backend profile. Hive forwards
changes only from the pilot of an idle session.

Use **Session Mode** in the browser or `Ctrl-G` in the TUI to select an advertised
primary agent, such as Build or Plan. Custom primary agents can also appear;
Hive preserves the catalog rather than hard-coding two modes. Model and mode
can both be changed between turns after the session has started.

Source inspection of OpenCode 1.19.4 confirms mode discovery through
`configOptions` and native `session/set_config_option`. Older legacy-only agents
use Hive's `session/set_mode` bridge. The selected value is agent-owned, not
Hive's backend default or an inference from a plan. Agent-reported changes
refresh attached clients; OpenCode versions differ in which external changes
produce notifications. The no-inference settings probe passed with installed
OpenCode 1.19.4 on 2026-10-26, including change, restoration, and attachment.
See the [mode compatibility reference](../compatibility.md#session-modes-and-agent-settings).

### Provider errors

An expired-key error requires creating a new API key in OpenCode Console and
replacing it in the client or secret store. Signing in again does not renew an
expired key. Changing the main model to another configured provider can avoid
that provider's failure, but auxiliary title/small-model calls may still use
their separately configured provider. Check OpenCode's configuration and logs
if those calls continue to fail. Hive displays controlled error guidance rather
than raw provider payloads; unrecognized failures direct you to those logs.

## Server-backed sessions

Start an OpenCode server separately, with its model/provider configured. Set
`OPENCODE_SERVER_PASSWORD` in the server's environment before starting it:

```sh
opencode serve --port 4096
```

Make the same password available in the Hive daemon's environment, and add:

```toml
[agents.opencode-server]
kind = "opencode-server"
endpoint = "http://127.0.0.1:4096"
username = "opencode"
password_env = "OPENCODE_SERVER_PASSWORD"
```

`password_env` contains the variable's name, not its value. Hive does not launch
or configure the server. Restart Hive after changing profiles and run
`hive doctor opencode-server` to check health, credentials, and version support.

The retained real-server probe covers OpenCode 1.19.3 on v1 routes. The adapter
rejects reported versions below 1.19.0. The `/api/*` preview has separate stores
and narrower evidence. See the [server capability matrix](../compatibility.md#capability-and-version-matrix).

## Discover and adopt a native session

Native adoption is currently an integration/API workflow. The supplied clients
do not have a native-session adoption picker or CLI subcommand.

First list candidates for the actual project directory:

```sh
curl --get 'http://127.0.0.1:8765/v1/providers/opencode-server/native-sessions' \
  --data-urlencode 'directory=/absolute/path/to/project'
```

Then send the following `session/new` parameters through an ACPv2 client on
`/acp`, substituting the native ID from discovery:

```json
{
  "cwd": "/absolute/path/to/project",
  "mcpServers": [],
  "_meta": {
    "hive": {
      "agent": "opencode-server",
      "adopt": { "nativeId": "ses_replace_me" }
    }
  }
}
```

This is an ACP request payload, not a REST POST to the discovery route. Both
`agent` and `adopt` are required here, even if a default profile exists.

Adoption reuses the native conversation without creating another. Duplicate
protection is scoped to the configured profile and native ID. Avoid configuring
multiple profiles as independent controllers of the same native session.

The returned Hive ID can be opened in ordinary Hive clients. Up to 50 recent
native messages feed the in-memory transcript snapshot. An ACP client must resume from
`start` to read that backfill. Older history remains in OpenCode.

Known bounded-import omissions are reported through Hive's shared `transcript`
status as partial, including preview history pagination budget exhaustion. This
status is independent of session recovery; no omitted entries does not certify
complete retention. ACP profiles use only negotiated ACP capabilities, without
an OpenCode-specific restoration policy.

## Continue in OpenCode

The catalog exposes `nativeId` for server-backed sessions, including sessions
Hive created. Use it with OpenCode's native attach command:

```sh
opencode attach http://127.0.0.1:4096 -s ses_replace_me
```

Hive cannot lock native clients out. It detects native foreground work and
refuses its own prompts while that work is active. This is detection, not
mutual exclusion across all OpenCode clients.

Closing the Hive session detaches Hive without deleting the native conversation.
Recovery preserves the original native ID and API flavor. A missing native
session is reported lost rather than replaced.

See [ADR-002](../adr/002-opencode-server-adoption.md) and the
[adapter reference](../compatibility.md#opencode-server-adapter) for exact
protocol, API-flavor, security, and diagnostic details.

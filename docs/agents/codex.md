# Codex

Hive can use the external
[`@agentclientprotocol/codex-acp`](https://github.com/agentclientprotocol/codex-acp)
adapter over stdio. It starts Codex App Server. Install and authenticate the
adapter separately; use the current package rather than the deprecated
`@zed-industries/codex-acp`. With an installed adapter:

```toml
[agents.codex]
name = "Codex"
command = "codex-acp"
versions = ["v2"]
```

Run `hive doctor codex` to verify your installation's wire version. Source
inspection of adapter 2.2.2 confirms ACPv1/v2 and two independent settings:

- **Mode** (`mode`): approval/sandbox preset. The advertised values include
  `read-only`, `workspace-write`, `agent`, and `agent-full-access`.
- **Collaboration mode** (`collaboration_mode`): `default` or `plan` for
  subsequent turns. This is the native Plan-mode equivalent.

Hive preserves both selectors and their meanings. Read-only approval settings
are not synonymous with planning. Use TUI `Ctrl-G` for these settings and
`Ctrl-P` for models, or the labelled browser controls. All remain changeable
between turns in an existing session.

These findings are source-verified, not live-agent-verified. See
[session configuration compatibility](../acp-compatibility.md#session-modes-and-agent-settings)
for notification and recovery limitations.

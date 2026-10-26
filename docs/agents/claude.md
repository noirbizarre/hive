# Claude Code

Hive can use the external
[`claude-agent-acp`](https://github.com/agentclientprotocol/claude-agent-acp)
adapter, powered by the Claude Agent SDK. Install and authenticate it separately.
For example, with an installed adapter:

```toml
[agents.claude]
name = "Claude Code"
command = "claude-agent-acp"
versions = ["v1"]
```

Run `hive doctor claude` and verify the declared wire version against your
installation. Source inspection of `@agentclientprotocol/claude-agent-acp`
0.96.0 confirms a native `mode` configuration selector for permission modes,
including Plan. Available choices are runtime-dependent; Hive does not invent
or enable unavailable permission modes.

Use **Session settings** (`Ctrl-G`) in the TUI or the labelled browser selector.
Models retain the dedicated `Ctrl-P` picker. Both remain changeable between
turns. Native configuration takes precedence over duplicate legacy modes.

These findings are source-verified, not live-agent-verified. See
[session configuration compatibility](../acp-compatibility.md#session-modes-and-agent-settings)
for state propagation and recovery behavior.

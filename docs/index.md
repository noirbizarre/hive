# Hive

Hive is a local gateway and session multiplexer for AI coding agents. One daemon
owns the conversations. You can start in the terminal, observe in a browser,
and take control from Zellij without starting another copy of the agent session.

## Start here

**[Get started](getting-started.md)** to install Hive from source, configure an
agent, and run your first project conversation. A credential-free mock option
lets you try the workflow without a model account.

| I want to… | Read |
| --- | --- |
| Configure profiles, credentials, or pricing | [Configuration](configuration.md) |
| Move a conversation between clients | [Sessions and handoff](guides/sessions.md) |
| Learn terminal controls | [TUI guide](guides/tui.md) |
| Continue in a browser | [Browser guide](guides/web.md) |
| Use a Zellij plugin | [ZJHive guide](guides/zellij.md) |
| Continue a native OpenCode conversation | [OpenCode integration](agents/opencode.md) |
| Diagnose setup or recovery | [Troubleshooting](troubleshooting.md) |
| Understand the system | [Architecture and diagrams](architecture.md) |

## What Hive provides

- A shared session catalog with stable Hive IDs.
- One pilot per session, with read-only observers and transferable control.
- ACPv2 clients over WebSocket or HTTP/SSE, with explicit downstream adapters.
- Bounded replay for client reconnect, durable recovery bookkeeping, and usage reporting.
- Terminal, browser, and Zellij clients using the same daemon-owned sessions.

Agent capabilities and real-world verification vary. Check the
[compatibility evidence](acp-compatibility.md) rather than assuming every profile
supports every operation.

## Current boundaries

Hive serves one trusted local OS user on loopback. It is not an authenticated
remote multi-user service. ACPv2 and its SDK interfaces are draft.

Client detach leaves work running, but daemon restart requires downstream
recovery support. Replay is bounded and not durable conversation history.
See [operations](operations.md) and the [status/roadmap](roadmap.md) for details.

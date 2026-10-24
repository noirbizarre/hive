# ZJHive in Zellij

The Zellij integration now lives in the independent
[ZJHive repository](https://github.com/noirbizarre/zjhive).
It owns the WASI plugin, native transport companion, installation, packaging,
compatibility documentation and live-host smoke tests.

Start Hive and create a session using the [TUI](tui.md) or [browser](web.md), then
follow ZJHive's build/load instructions. Hive's workspace and CLI no longer contain
or launch a Zellij-specific helper. Install matching plugin/companion artifacts
from ZJHive instead of using the former in-tree plugin and helper command.

Hive remains the authoritative session owner. ZJHive uses its operational catalog
and existing [ACP interfaces](../transports.md), with the same control/replay
contract as other clients. See [sessions and handoff](sessions.md).

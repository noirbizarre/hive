# Pi

Pi has no native ACP mode. Hive reaches it through the external
[`pi-acp`](https://github.com/svkozak/pi-acp) adapter, which launches Pi's RPC
mode and translates it to ACPv1.

## Prerequisites

Install Pi and configure its provider/model outside Hive. The adapter documents
Node.js 22+ and Pi 0.81.0+ as minimums. Hive's recorded initialize probe used
Pi 1.3.0, `pi-acp` 0.0.34, and Node 26 on 2026-10-23.

Authenticate in Pi itself or with `pi-acp --terminal-login` when using an
installed adapter. Hive does not supply credentials or maintain the adapter.

## Profile

```toml
[agents.pi]
name = "Pi"
command = "npx"
args = ["-y", "pi-acp@0.0.34"]
versions = ["v1"]
```

Alternatively, install the tested adapter version yourself and use
`command = "pi-acp"` without the `npx` arguments. Keep the version pinned to the
one you verified. Do not configure bare `pi` or treat `pi --mode rpc` as ACP.

Run `hive doctor pi` before creating a session. The recorded adapter answers
initialize with protocol version 1, so declaring only v2 fails.

## Evidence and limitations

Only initialization is recorded as verified for this installation. Full prompt
turns, streaming, tool events, cancellation, and reconnect remain unverified.
The [compatibility matrix](../acp-compatibility.md#installed-agent-capability-matrix-2026-10-11)
and [Pi protocol notes](../acp-compatibility.md#pi) retain the exact evidence.

The adapter runs tools itself rather than delegating filesystem/terminal work
to Hive. Nonempty MCP declarations are not supported by Hive's v1 adapter.
Pi extension slash commands are not available through this path.

Source inspection of `pi-acp` 0.0.34 shows model and **thinking level** session
configuration. Use `Ctrl-P` for models and `Ctrl-G` for thinking level in the
TUI, or the labelled browser controls, including between later turns. The
adapter also advertises legacy modes for the same thinking levels; Hive prefers
its native configuration to avoid a duplicate mode selector. Thinking level is
not a Plan/Build mode, and `--mode rpc` selects Pi's transport. Live settings
changes remain unverified for this installation.

Native Pi-session adoption is separate planned work. An adapter advertising
session loading does not mean Hive can import arbitrary native Pi sessions.
See [development probes](../development.md#real-agent-compatibility-probes)
before upgrading compatibility claims.

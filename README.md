<h1 align="center">
  <img src="docs/images/logo.svg" alt="hive" width="640">
</h1>

<p align="center"><strong>An ACPv2 gateway and session multiplexer for AI coding agents</strong></p>

<p align="center">
  <a href="https://github.com/noirbizarre/hive/actions/workflows/ci.yaml">
    <img src="https://github.com/noirbizarre/hive/actions/workflows/ci.yaml/badge.svg" alt="CI">
  </a>
  <a href="https://codecov.io/gh/noirbizarre/hive">
    <img src="https://codecov.io/gh/noirbizarre/hive/graph/badge.svg" alt="Codecov">
  </a>
  <a href="https://noirbizarre.github.io/hive/">
    <img src="https://img.shields.io/badge/docs-noirbizarre.github.io-blue" alt="Documentation">
  </a>
  <img src="https://img.shields.io/github/license/noirbizarre/hive" alt="License">
</p>

Hive runs one local daemon that owns shared agent sessions. Start a conversation
in the TUI, observe it in the Web UI, and transfer control between ACP clients without
duplicating the downstream conversation. Sessions outlive client disconnects.

Clients speak ACPv2 over WebSocket or HTTP/SSE. Configured agents use explicit
ACPv1/v2 adapters or the native OpenCode server adapter. One pilot controls a
session while observers follow its updates.

The ACPv2 specification and SDK APIs are **draft**. Hive currently serves one
trusted local user on loopback, without remote multi-user authentication.

## Installation

With Rust installed, run this from a Hive checkout:

```sh
cargo install --path crates/hive-cli --bin hive
```

The checkout uses Rust's `stable` channel and declares MSRV 1.88. Source
installation includes the terminal client and embedded browser assets.

## Quickstart

Install and authenticate OpenCode separately, with a working model configured.
Create `${XDG_CONFIG_HOME:-$HOME/.config}/hive/config.toml`:

```toml
default_agent = "opencode"

[agents.opencode]
name = "OpenCode"
command = "opencode"
args = ["acp"]
versions = ["v1"]
```

Confirm the entrypoint and wire version match your installed agent. Then run:

```sh
hive doctor opencode
hive daemon
```

Leave the daemon running. In another terminal, from your project directory:

```sh
hive daemon status
hive tui --agent opencode
```

Press `Ctrl-N`, select the profile, and press Enter to create a session. Type a
prompt and press Enter. Run `hive web` and select the same session to continue
in the browser. Typing there requests control; `Ctrl-T` takes control back in
the TUI. Esc leaves the session running.

**[Read the getting-started guide](docs/getting-started.md)** for prerequisites,
expected results, shutdown/recovery behavior, and a credential-free mock option.

## Documentation

Full site: <https://noirbizarre.github.io/hive/>

- [Configuration](docs/configuration.md) and [CLI reference](docs/reference/cli.md)
- [Sessions and handoff](docs/guides/sessions.md)
- [TUI](docs/guides/tui.md) and [Web UI](docs/guides/web.md)
- [OpenCode](docs/agents/opencode.md) and [Pi](docs/agents/pi.md) integrations
- [Operations](docs/operations.md) and [troubleshooting](docs/troubleshooting.md)
- [Architecture and diagrams](docs/architecture.md), [decisions](docs/adr/README.md), and [roadmap](docs/roadmap.md)
- [ACP compatibility evidence](docs/acp-compatibility.md), [transports](docs/transports.md), and [HTTP API](docs/hive-api.md)

Replay is bounded, not a durable transcript. Restart recovery depends on the
downstream agent and never silently resends interrupted prompts.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and the [development guide](docs/development.md).
Run `mise run ci` for the repository-wide local checks. Browser checks
have a separate task documented in the development guide.

## License

MIT — see [LICENSE](LICENSE).

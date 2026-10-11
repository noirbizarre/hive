<p align="center">
  <img src="docs/images/icon.svg" alt="hive" width="160">
</p>

<h1 align="center">hive</h1>

<p align="center"><strong>An ACPv2 gateway and session multiplexer for AI coding agents</strong></p>

<p align="center">
  <a href="https://github.com/noirbizarre/hive/actions/workflows/ci.yaml">
    <img src="https://github.com/noirbizarre/hive/actions/workflows/ci.yaml/badge.svg" alt="CI">
  </a>
  <a href="https://codecov.io/gh/noirbizarre/hive">
    <img src="https://codecov.io/gh/noirbizarre/hive/graph/badge.svg" alt="Codecov">
  </a>
  <img src="https://img.shields.io/github/v/release/noirbizarre/hive" alt="Release">
  <a href="https://noirbizarre.github.io/hive/">
    <img src="https://img.shields.io/badge/docs-noirbizarre.github.io-blue" alt="Documentation">
  </a>
  <img src="https://img.shields.io/github/license/noirbizarre/hive" alt="License">
</p>

---

Hive provides one ACPv2 gateway for multiple clients and manages configured
ACPv1 or ACPv2 agents behind it. One local daemon exposes the official
ACP HTTP/SSE and WebSocket transports; clients do not launch agents themselves.

The ACPv2 specification and SDK APIs are **draft**. This bootstrap is intended
for a single trusted local user, not a remotely accessible multi-user service.

## Installation

Install Rust (the pinned stable toolchain), then `mise install` for the template
tools. Build with `mise run build` or `cargo build --workspace`.

Create `$XDG_CONFIG_HOME/hive/config.toml` (or `~/.config/hive/config.toml`):

```toml
default_agent = "opencode"

[agents.opencode]
name = "OpenCode"
command = "opencode"
args = ["acp"]
versions = ["v1"] # Confirm the ACP wire version for your installation.
```

The commands and version declarations are examples; check your installed
agents' ACP entrypoints and supported versions. Pi 1.1.0 does not advertise
an ACP mode by default; configure Pi only after verifying an ACP-capable
extension/entrypoint. Profiles may also specify
`env = { KEY = "value" }`; an endpoint profile uses
`endpoint = "http://127.0.0.1:9000"` (or `ws://...`) and optional
`headers = { Authorization = "Bearer ..." }`. Keep the config file private.
No default is required to start Hive; without one, each session must select a
profile. `HIVE_DEFAULT_AGENT` overrides `default_agent`, `HIVE_CONFIG` or
`--config` chooses a different file, and `HIVE_LISTEN` overrides the loopback
listen address.

Start the daemon once:

```sh
cargo run -p hive-daemon --bin hive -- serve
```

In another terminal run `cargo run -p hive-daemon --bin hive -- status` and
`cargo run -p hive-tui`. The web client is served at
<http://127.0.0.1:8765/hive/v1/web>. Build the Zellij plugin separately with
`cargo build -p hive-zellij --target wasm32-wasip1`, then load
`target/wasm32-wasip1/debug/hive_zellij.wasm` through Zellij's plugin loader;
grant its WebAccess permission. Configure its `url` setting for a non-default
loopback daemon. The full-screen TUI lists sessions in the current directory.
Press `n`/`Ctrl-N` to create one, select a profile with the arrow keys and Enter,
then type prompts in the conversation pane. `--agent opencode` preselects a
profile; `--session hive-…` resumes a known session. `Tab` shows the session
list, `a` toggles all directories, Enter takes pilot control and `o` attaches
as an observer. `Ctrl-H` relinquishes pilot control while continuing to
observe; `Ctrl-T` takes control explicitly. `Ctrl-C` cancels work, `Esc` quits
without closing the session, and `Ctrl-R` accepts a live-only reconnect if
replay history expires. Permission requests present numbered choices; form
input accepts comma-separated `key=value` fields and URL requests show a URL
to open outside the TUI. The web client offers an agent selector before its
first session. Other clients
send `session/new` with `_meta: {"hive":{"agent":"opencode"}}`, or omit it
to select the configured default. Query `/hive/v1/providers` for agent IDs.

For a key-free local smoke test, build `hive-mock-agent`, set an agent profile
with `command = "./target/debug/hive-mock-agent"` and `versions = ["v2"]`,
then run the same `hive serve` command.

Tests: `mise run test`, `mise run cover`, and `mise run ci`. The focused
end-to-end test is `cargo test -p hive-daemon --test gateway`.
Installed-agent tests and native-session probes are opt-in; see
[real-agent compatibility probes](docs/development.md#real-agent-compatibility-probes)
and the [versioned capability matrix](docs/acp-compatibility.md#installed-agent-capability-matrix-2026-10-11).

## Documentation

<https://noirbizarre.github.io/hive/>

See [architecture](docs/architecture.md), [development](docs/development.md),
[ACP compatibility](docs/acp-compatibility.md), [transport matrix](docs/transports.md)
and [Hive API](docs/hive-api.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT — see [LICENSE](LICENSE).

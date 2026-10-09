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

Hive provides one ACPv2 gateway for multiple clients and translates configured
ACPv1 or ACPv2 stdio agents behind it. One local daemon exposes the official
ACP HTTP/SSE and WebSocket transports; clients do not launch agents themselves.

The ACPv2 specification and SDK APIs are **draft**. This bootstrap is intended
for a single trusted local user, not a remotely accessible multi-user service.

## Installation

Install Rust (the pinned stable toolchain), then `mise install` for the template
tools. Build with `mise run build` or `cargo build --workspace`.

Start a deterministic local agent without an API key:

```sh
cargo build -p hive-mock-agent
cargo run -p hive-daemon --bin hive -- serve --agent ./target/debug/hive-mock-agent
```

In another terminal run `cargo run -p hive-daemon --bin hive -- status` and
`cargo run -p hive-tui`. The web client is served at
<http://127.0.0.1:8765/hive/v1/web>. Build the Zellij plugin separately with
`cargo build -p hive-zellij --target wasm32-wasip1`, then load
`target/wasm32-wasip1/debug/hive_zellij.wasm` through Zellij's plugin loader;
grant its WebAccess permission. Configure its `url` setting for a non-default
loopback daemon. To use a declared v1 stdio provider instead, start `hive serve
--agent 'your-agent-command' --agent-version v1`.

Tests: `mise run test`, `mise run cover`, and `mise run ci`. The focused
end-to-end test is `cargo test -p hive-daemon --test gateway`.

## Documentation

<https://noirbizarre.github.io/hive/>

See [architecture](docs/architecture.md), [development](docs/development.md),
[ACP compatibility](docs/acp-compatibility.md), [transport matrix](docs/transports.md)
and [Hive API](docs/hive-api.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT — see [LICENSE](LICENSE).

# ZJHive in Zellij

ZJHive opens existing Hive sessions inside Zellij. A native helper maintains the
ACP connection because the plugin host exposes one-shot HTTP requests rather
than persistent WebSocket/SSE streams.

## Build and load

Install `hive` on `PATH`, then build the plugin from the checkout:

```sh
rustup target add wasm32-wasip1
cargo build -p hive-zellij --target wasm32-wasip1
```

Load `target/wasm32-wasip1/debug/hive_zellij.wasm` with Zellij's plugin loader.
The plugin SDK dependency is pinned to `zellij-tile` 0.45.1. Grant **WebAccess**
and **RunCommands** when requested.

Start the daemon and create a session in the TUI first. The plugin starts
`hive zellij-bridge` on demand, so the installed executable must be available
to Zellij's environment.

## Connection settings

| Plugin setting | Default | Purpose |
| --- | --- | --- |
| `url` | `http://127.0.0.1:8765` | Hive daemon base URL |
| `bridge` | `http://127.0.0.1:8766` | Native helper base URL |

Set these in the plugin configuration when using non-default ports. If managing
the helper yourself, its corresponding command is:

```sh
hive zellij-bridge --listen 127.0.0.1:8766 --url http://127.0.0.1:8765
```

The helper is another Hive client, not another daemon or session manager.

## Controls

| Key | Action |
| --- | --- |
| Arrows, Enter | Select an existing session and observe |
| `Ctrl-T` | Take control |
| Enter while composing | Send typed text |
| `Ctrl-C` | Cancel work |
| `Ctrl-H` | Relinquish control |
| `Ctrl-R` | Retry an uncertain prompt or recover expired history |
| Esc | Detach |

Permissions use explicit numbered choices. Forms support field navigation and
validated submission. URL questions distinguish consent from completion.
Use the actual control-key bindings above even if an older status message
suggests bare `t` or `h`.

## Lifecycle and limits

Each plugin registration gets a separate ACP connection. Reloading/detaching the
plugin or stopping the helper drops that client connection without closing the
daemon-owned session. Idle bridge handles expire after about 30 seconds.

The plugin polls the catalog and bounded conversation snapshots. It does not
create sessions or keep a durable transcript. It also lacks the interrupted-work
acknowledgement action: use the TUI or browser to acknowledge recovery before
returning to ZJHive.

See [ADR-004](../adr/004-zellij-native-acp-bridge.md) for the host boundary and
[development](../development.md#live-zellij-smoke-test) for live-host validation.
